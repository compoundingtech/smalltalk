# Synthetic installed harness for admission tests. The measured capture supplies event evidence;
# this executable exercises the real scratch launch, loopback model, and identity cache paths.
import json, os, pathlib, re, sys, urllib.request
if '--version' in sys.argv:
    print('omp/99.42.7')
    sys.exit(0)
root = pathlib.Path(__file__).parent
if 'ST_ADMISSION_TRACE' not in os.environ:
    (root / 'provider-env').write_text(os.environ.get('ST_OMP_CHANNEL_EXPECTED_NATIVE_SESSION', 'unset') + '|' + os.environ.get('ST_OMP_CHANNEL_RESUME_GENERATION', 'unset') + '\n')
    sys.exit(0)
(root / 'launches').open('a').write('probe\n')
assert os.environ['ST_AGENT'] == 'admission.fixture'
session_dir = pathlib.Path(sys.argv[sys.argv.index('--session-dir') + 1])
assert session_dir.is_dir()
assert session_dir.parent == pathlib.Path(os.environ['HOME']).parent
assert 'ANTHROPIC_API_KEY' not in os.environ
assert pathlib.Path(os.environ['HOME']).parent == pathlib.Path(os.environ['ST_ADMISSION_TRACE']).parent
assert pathlib.Path.cwd().name == 'workspace'
assert (pathlib.Path(os.environ['HOME']) / '.omp/agent/models.json').is_file()
if (root / 'slow').exists():
    import time
    time.sleep(60)
fixture = json.loads((root / 'capture.json').read_text())
nonce = re.search(r'ADMISSION_NATIVE_[0-9-]+', pathlib.Path(os.environ['ST_OMP_CHANNEL_BIN']).read_text())[0]
fixture = json.loads(json.dumps(fixture).replace(fixture['nonce'], nonce))
profile = pathlib.Path(os.environ.get('PI_CODING_AGENT_DIR', str(pathlib.Path(os.environ['HOME']) / '.omp/agent')))
endpoint = json.loads((profile / 'models.json').read_text())['providers']['admission']['baseUrl']
messages = [{'role': 'user', 'content': nonce}]
for _ in range(2):
    payload = json.dumps({'messages': messages}).encode()
    with urllib.request.urlopen(urllib.request.Request(endpoint + '/chat/completions', payload, {'Content-Type': 'application/json'})) as response:
        response.read()
    messages.append({'role': 'tool', 'tool_call_id': 'admission_call', 'content': 'fixture denied'})
for name, key in [('ST_ADMISSION_TRACE', 'events'), ('ST_ADMISSION_CHANNEL_TRACE', 'channel')]:
    pathlib.Path(os.environ[name]).write_text(''.join(json.dumps(value) + '\n' for value in fixture[key]))
if (root / 'malformed').exists():
    with pathlib.Path(os.environ['ST_ADMISSION_TRACE']).open('a') as output:
        output.write('malformed\n')
