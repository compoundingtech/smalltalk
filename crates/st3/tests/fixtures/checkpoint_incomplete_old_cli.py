"""Pinned old CLI wire probe, deliberately separate from real-router Rust tests.

python3 checkpoint_incomplete_old_cli.py /path/to/old/st3 EXPECTED_SHA256
One private Unix listener returns the exact new incomplete-evidence response.
The probe refuses an unpinned executable and records the one-request/nonzero result.
"""
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading

binary = Path(sys.argv[1]).resolve(strict=True)
expected = sys.argv[2]
actual = hashlib.file_digest(binary.open('rb'), 'sha256').hexdigest()
assert actual == expected, (actual, expected)
body = json.dumps({
    'code': 'diagnostic-evidence-incomplete',
    'message': 'checkpoint evidence incomplete; the current set is not certified; this read does not start an audit',
    'details': {'comparison_state': 'uncomputed'},
}).encode()
requests = []
errors = []
with tempfile.TemporaryDirectory(prefix='st3-old-cli-wire-') as name:
    root = Path(name)
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    endpoint = root / 'daemon.sock'
    listener.bind(str(endpoint))
    listener.listen(2)
    listener.settimeout(8)

    def serve():
        try:
            connection, _ = listener.accept()
            with connection:
                connection.settimeout(5)
                request = b''
                while b'\r\n\r\n' not in request:
                    request += connection.recv(4096)
                    assert len(request) <= 65536
                requests.append(request.split(b'\r\n', 1)[0].decode())
                connection.sendall(b'HTTP/1.1 503 Service Unavailable\r\n'
                    + b'Content-Type: application/json\r\nConnection: close\r\nContent-Length: '
                    + str(len(body)).encode() + b'\r\n\r\n' + body)
        except BaseException as error:
            errors.append(str(error))

    worker = threading.Thread(target=serve, daemon=True)
    worker.start()
    environment = dict(os.environ)
    for key in ['ST_AGENT', 'ST_MISSION_RUN', 'ST3_MESSAGE_ROOT', 'ST3_ENDPOINT']:
        environment.pop(key, None)
    for key in ['XDG_CONFIG_HOME', 'XDG_STATE_HOME', 'XDG_RUNTIME_DIR']:
        directory = root / key.lower()
        directory.mkdir(mode=0o700)
        environment[key] = str(directory)
    try:
        answer = subprocess.run([str(binary), '--endpoint', str(endpoint),
            'replication', 'checkpoint', 'status'], env=environment,
            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=7)
        worker.join(timeout=1)
        assert not worker.is_alive(), 'wire fixture did not complete'
        assert not errors, errors
        assert requests == ['GET /v1/checkpoint/status HTTP/1.1'], requests
        assert answer.returncode != 0, answer
        output = answer.stdout + answer.stderr
        assert 'evidence incomplete' in output, output
        assert 'healthy' not in output.lower(), output
        print(json.dumps({'binary_sha256': actual, 'requests': requests,
            'exit': answer.returncode, 'stdout': answer.stdout, 'stderr': answer.stderr,
            'scope': 'Mock wire compatibility; real router and middleware are tested separately.'}))
    finally:
        listener.close()
