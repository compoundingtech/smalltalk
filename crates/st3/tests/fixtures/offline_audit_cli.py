"""Exercise offline CLI isolation and real SIGINT/SIGTERM cleanup on private evidence.

Run through builds-slice-run: python3 offline_audit_cli.py /path/to/new/st3.
No live store or daemon is opened. All input, configuration and scratch files are private.
"""
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time


binary = Path(sys.argv[1]).resolve(strict=True)
results = []
with tempfile.TemporaryDirectory(prefix="st3-offline-cli-test-") as name:
    root = Path(name)
    environment = dict(os.environ)
    for key in ["ST_AGENT", "ST_MISSION_RUN", "ST3_MESSAGE_ROOT", "ST3_ENDPOINT"]:
        environment.pop(key, None)
    for key in ["XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR"]:
        directory = root / key.lower()
        directory.mkdir(mode=0o700)
        environment[key] = str(directory)
    endpoint = root / "never-contact.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(endpoint))
    listener.listen(4)
    listener.setblocking(False)
    evidence = root / "evidence.sqlite3"
    with sqlite3.connect(evidence) as db:
        db.execute("CREATE TABLE private_fixture(payload BLOB)")
        # Large enough to observe the descriptor copy before it finishes, without a live scan.
        db.executemany("INSERT INTO private_fixture VALUES(zeroblob(65536))", [()] * 1024)
    before = hashlib.file_digest(evidence.open("rb"), "sha256").hexdigest()
    for interruption in [signal.SIGINT, signal.SIGTERM]:
        scratch = root / f"scratch-{interruption.name}"
        scratch.mkdir()
        command = [str(binary), "--endpoint", str(endpoint), "--daemon-wait", "0",
                   "doctor", "--offline-audit", str(evidence), "--audit-scratch-dir",
                   str(scratch), "--audit-max-bytes", "2147483648"]
        process = subprocess.Popen(command, env=environment, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            deadline = time.monotonic() + 15
            while not list(scratch.glob("st3-offline-command-*/st3-offline-audit-*/input.sqlite3")):
                assert process.poll() is None, process.communicate()
                assert time.monotonic() < deadline, "offline copy did not start"
                time.sleep(0.001)
            process.send_signal(interruption)
            stdout, stderr = process.communicate(timeout=15)
            assert process.returncode == 2, (process.returncode, stdout, stderr)
            assert not list(scratch.iterdir()), list(scratch.iterdir())
            assert hashlib.file_digest(evidence.open("rb"), "sha256").hexdigest() == before
            assert not Path(str(evidence) + "-shm").exists()
            assert not Path(str(evidence) + "-wal").exists()
            results.append({"signal": interruption.name, "exit": process.returncode,
                            "scratch_clean": True, "input_unchanged": True})
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate(timeout=5)
    try:
        connection, _ = listener.accept()
    except BlockingIOError:
        pass
    else:
        connection.close()
        raise AssertionError("offline audit contacted the daemon")
    listener.close()
print(json.dumps({"binary_sha256": hashlib.file_digest(binary.open("rb"), "sha256").hexdigest(),
                  "signals": results, "daemon_requests": 0}))
