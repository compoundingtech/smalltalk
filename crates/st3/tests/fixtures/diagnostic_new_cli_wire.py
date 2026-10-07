"""Check new CLI rendering and legacy success parsing through a private mock wire.

Run through builds-slice-run: python3 diagnostic_new_cli_wire.py /path/to/new/st3.
Actual router behavior is covered by the Rust diagnostic tests.
"""
import hashlib, json, os, socket, subprocess, sys, tempfile, threading
from pathlib import Path
binary = Path(sys.argv[1]).resolve()
checks = [{'name': name, 'status': 'unknown', 'message': 'evidence incomplete'} for name in ['claim-signatures', 'operation-projection', 'graph-references', 'replication', 'checkpoint-evidence']]
unknown = {'status': 'warn', 'checks': checks, 'performance': {}}
legacy = {'node': 'legacy-node', 'newest_stable': None, 'trimmed': None, 'halted': False, 'pending': None, 'participants': [], 'excused': [], 'left': []}
incomplete = {'code': 'diagnostic-evidence-incomplete', 'message': 'checkpoint evidence incomplete; the current set is not certified; this read does not start an audit', 'details': {'comparison_state': 'uncomputed'}}
scenarios = [('checkpoint-incomplete', ['replication', 'checkpoint', 'status'], 503, incomplete, 2), ('checkpoint-old-success', ['--json', 'replication', 'checkpoint', 'status'], 200, legacy, 0), ('strict-doctor-text', ['doctor', '--strict'], 200, unknown, 2), ('strict-doctor-json', ['--json', 'doctor', '--strict'], 200, unknown, 2)]
rows = []
for name, args, status, value, expected in scenarios:
    with tempfile.TemporaryDirectory(prefix='st3-new-cli-wire-') as tmp:
        root = Path(tmp)
        endpoint = root / 'daemon.sock'
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(endpoint))
        listener.listen(2)
        listener.settimeout(8)
        requests = []
        errors = []

        def serve():
            try:
                conn, _ = listener.accept()
                with conn:
                    conn.settimeout(5)
                    req = b''
                    while b'\r\n\r\n' not in req:
                        chunk = conn.recv(4096)
                        assert chunk
                        req += chunk
                        assert len(req) < 65536
                    requests.append(req.split(b'\r\n', 1)[0].decode())
                    data = json.dumps({'api_version': 'st3.v1', 'value': value} if status == 200 else value).encode()
                    conn.sendall(f'HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {len(data)}\r\n\r\n'.encode() + data)
            except BaseException as e:
                errors.append(repr(e))
        worker = threading.Thread(target=serve, daemon=True)
        worker.start()
        env = dict(os.environ)
        for key in ['ST_AGENT', 'ST_MISSION_RUN', 'ST3_MESSAGE_ROOT', 'ST3_ENDPOINT']:
            env.pop(key, None)
        for key in ['XDG_CONFIG_HOME', 'XDG_STATE_HOME', 'XDG_RUNTIME_DIR']:
            path = root / key.lower()
            path.mkdir()
            env[key] = str(path)
        try:
            p = subprocess.run([str(binary), '--endpoint', str(endpoint),  *args], env=env, capture_output=True, text=True, timeout=7)
            worker.join(1)
            assert not worker.is_alive()
            assert not errors, errors
            assert len(requests) == 1, requests
            assert p.returncode == expected, (name, p)
            if name == 'checkpoint-incomplete':
                assert p.stderr.count('uncomputed: checkpoint evidence incomplete') == 1 and (not p.stdout), p
            if name == 'checkpoint-old-success':
                assert json.loads(p.stdout) == legacy, p
            if name == 'strict-doctor-text':
                for check in checks:
                    assert 'unknown\t' + check['name'] in p.stdout, p
            if name == 'strict-doctor-json':
                assert json.loads(p.stdout)['checks'] == checks, p
            rows.append({'name': name, 'exit': p.returncode, 'requests': requests, 'stdout': p.stdout, 'stderr': p.stderr})
        finally:
            listener.close()
print(json.dumps({'binary_sha256': hashlib.file_digest(binary.open('rb'), 'sha256').hexdigest(), 'scope': 'mock wire CLI compatibility, separate from router tests', 'results': rows}))
