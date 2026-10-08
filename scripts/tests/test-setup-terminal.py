#!/usr/bin/env python3
"""Prove a caller-supplied setup command stays unsent until Enter in the real TUI.

Usage: python3 scripts/tests/test-setup-terminal.py EXAMPLE_BINARY DAEMON_BINARY PTY_BINARY
The example is built with cargo build -p stui --example setup_terminal.
All state, terminal sessions and the sentinel belong to a temporary fixture.
"""
import json
import fcntl
import os
from pathlib import Path
import pty
import re
import select
import shlex
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time


def main():
    example, daemon_bin, pty_bin = map(lambda p: str(Path(p).resolve()), sys.argv[1:])
    with tempfile.TemporaryDirectory(prefix="st-setup-") as directory:
        root = Path(directory)
        (root / "config/st3").mkdir(parents=True)
        (root / "config/st3/config.toml").write_text(
            f'node = "studio"\nperson = "person/ada"\nstate_dir = "{root}/state"\n'
            f'socket = "{root}/api.sock"\nclient_gateway_socket = "{root}/gateway.sock"\n'
        )
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("ST_", "ST3_", "STUI_", "PTY_"))}
        env.update(HOME=str(root), XDG_CONFIG_HOME=str(root / "config"),
                   XDG_CACHE_HOME=str(root / "cache"), XDG_STATE_HOME=str(root / "state"),
                   ST3_ENDPOINT=str(root / "api.sock"), ST3_PERSON="person/ada",
                   SHELL="/bin/sh", TERM="xterm-256color", PS1="setup-fixture$ ",
                   STUI_TIMING_LOG=str(root / "timing.jsonl"))
        daemon = None
        ui = None
        master = None
        output = bytearray()
        with (root / "daemon.log").open("wb") as log:
            try:
                daemon = subprocess.Popen([daemon_bin, "up", "--pty-binary", pty_bin],
                                          env=env, stdout=log, stderr=log)
                deadline = time.monotonic() + 20
                while not (root / "gateway.sock").exists():
                    assert daemon.poll() is None, (root / "daemon.log").read_text()
                    assert time.monotonic() < deadline, "daemon socket did not appear"
                    time.sleep(.05)
                sentinel = root / "executed"
                command = f"printf setup-done > {shlex.quote(str(sentinel))}"
                master, slave = pty.openpty()
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
                before = termios.tcgetattr(slave)
                # Unix authentication binds an agent's descendants to that seat even after
                # clearing ST_AGENT. A fixture representing a person needs an independent
                # supervisor; it connects exclusively to this fixture's private API socket.
                supervisor = """
import os, subprocess, sys
if os.fork():
    sys.exit(0)
os.setsid()
process = subprocess.Popen(sys.argv[3:])
open(sys.argv[1] + '.tmp', 'w').write(str(process.pid))
os.rename(sys.argv[1] + '.tmp', sys.argv[1])
code = process.wait()
open(sys.argv[2] + '.tmp', 'w').write(str(code))
os.rename(sys.argv[2] + '.tmp', sys.argv[2])
"""
                launcher = subprocess.Popen([sys.executable, "-c", supervisor,
                                             str(root / "ui.pid"), str(root / "ui.exit"), example, command],
                                            stdin=slave, stdout=slave, stderr=slave, env=env)
                assert launcher.wait(5) == 0

                class FixtureUi:
                    def poll(self):
                        return int((root / "ui.exit").read_text()) if (root / "ui.exit").exists() else None
                    def send_signal(self, value):
                        os.kill(int((root / "ui.pid").read_text()), value)
                    def wait(self, seconds):
                        deadline = time.monotonic() + seconds
                        while self.poll() is None:
                            assert time.monotonic() < deadline, "UI did not exit"
                            time.sleep(.02)
                        return self.poll()
                    @property
                    def returncode(self):
                        return self.poll()

                ui = FixtureUi()
                queries = 0

                def pump(seconds):
                    nonlocal queries
                    deadline = time.monotonic() + seconds
                    while time.monotonic() < deadline:
                        assert ui.poll() is None, output.decode(errors="replace")
                        ready, _, _ = select.select([master], [], [], .05)
                        if ready:
                            chunk = os.read(master, 65536)
                            output.extend(chunk)
                            count = output.count(b"\x1b[c")
                            while queries < count:
                                os.write(master, b"\x1b[?62;c")
                                queries += 1

                deadline = time.monotonic() + 25
                # Unchanged blank cells are cursor jumps in ratatui's incremental output.
                # Match the unique sentinel path and command with whitespace removed.
                def rendered():
                    plain = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", output)
                    return re.sub(rb"\s+", b"", plain).decode(errors="replace")

                while re.sub(r"\s+", "", command) not in rendered():
                    assert time.monotonic() < deadline, ((root / "daemon.log").read_text()[-3000:] + re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", output).decode(errors="replace")[-3000:])
                    pump(.1)
                    assert not sentinel.exists(), "command executed during attachment"
                assert "dragselectsandcopies" in rendered(), "native terminal pane did not render"
                pump(1.5)
                assert not sentinel.exists(), "command executed while idle without Enter"
                os.write(master, b"\r")
                deadline = time.monotonic() + 5
                while not sentinel.exists():
                    assert time.monotonic() < deadline, "explicit Enter did not run staged command"
                    pump(.1)
                assert sentinel.read_text() == "setup-done"
                os.write(master, b"\x1c")  # Ctrl+\ leaves the attached shell.
                pump(.2)
                os.write(master, b"\x11")  # Ctrl+Q quits the UI.
                ui.wait(5)
                assert ui.returncode == 0
                assert termios.tcgetattr(slave) == before, "terminal modes were not restored"
                os.close(slave)
                print("PASS: setup tab rendered the staged command; idle did not execute; Enter executed; Ctrl+\\ then Ctrl+Q restored terminal modes.")
            finally:
                if ui and ui.poll() is None:
                    ui.send_signal(signal.SIGTERM)
                    ui.wait(5)
                # Stop only the fixture's PTY sessions before removing its registry.
                registry = {**env, "PTY_ROOT": str(root / "state/pty")}
                listing = subprocess.run([pty_bin, "ls", "--json"], env=registry, capture_output=True)
                if listing.returncode == 0:
                    for session in json.loads(listing.stdout):
                        subprocess.run([pty_bin, "kill", session["name"]], env=registry,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                if daemon and daemon.poll() is None:
                    daemon.terminate()
                    daemon.wait(10)
                if master is not None:
                    os.close(master)


if __name__ == "__main__":
    main()
