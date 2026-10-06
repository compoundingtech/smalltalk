# Synthetic future server for testing the real installed-executable admission path.
import base64, http.server, json, os, pathlib, queue, sys, threading, urllib.request
root = pathlib.Path(__file__).parent
if '--version' in sys.argv:
    print('opencode/99.42.7')
    sys.exit(0)
assert 'ST_AGENT' not in os.environ
assert 'ANTHROPIC_API_KEY' not in os.environ
(root / 'launches').open('a').write('probe\n')
failure = (root / 'failure').read_text() if (root / 'failure').exists() else ''
config = json.loads(os.environ['OPENCODE_CONFIG_CONTENT'])
endpoint = config['provider']['admission']['options']['baseURL']
events = queue.Queue()
messages = []
asks = []
nonce = ''
session = 'fixture-session'
call = 'admission_call'
ask = {'id': 'fixture-permission', 'sessionID': session, 'permission': 'bash',
       'metadata': {'command': 'printf ADMISSION_FIXTURE'}, 'tool': {'callID': call}}
def emit(kind, properties):
    events.put({'type': kind, 'properties': {'sessionID': session, **properties}})
def model(continuation):
    payload = {'messages': [{'role': 'user', 'content': nonce}]}
    if continuation:
        payload['messages'].append({'role': 'tool', 'tool_call_id': call, 'content': 'fixture done'})
    with urllib.request.urlopen(urllib.request.Request(endpoint + '/chat/completions', json.dumps(payload).encode(), {'Content-Type': 'application/json'})) as response:
        response.read()
class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.0'
    def log_message(self, *_):
        pass
    def json(self, data, status=200):
        body = json.dumps(data).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_GET(self):
        expected = 'Basic ' + base64.b64encode(('opencode:' + os.environ['OPENCODE_SERVER_PASSWORD']).encode()).decode()
        assert self.headers['Authorization'] == expected
        if self.path == '/doc':
            markers = ['prompt_async', 'messageID', 'session.status', 'session.idle', 'session.error', 'permission.asked', 'permission.replied', 'question.asked', 'question.replied', 'question.rejected', '/permission', '/question']
            if failure == 'apiContract':
                markers.remove('prompt_async')
            self.json({'markers': markers})
        elif self.path == '/session/status': self.json({})
        elif self.path == '/permission': self.json(asks)
        elif self.path == '/question': self.json([])
        elif self.path == '/session/' + session + '/message': self.json(messages)
        elif self.path == '/event':
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.end_headers()
            while True:
                try:
                    try: event = events.get(timeout=.05)
                    except queue.Empty: event = {'type': 'server.heartbeat'}
                    self.wfile.write(('data: ' + json.dumps(event) + '\n\n').encode())
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError): break
        else: self.json({}, 404)
    def do_POST(self):
        global nonce
        payload = json.loads(self.rfile.read(int(self.headers.get('Content-Length', '0'))))
        if self.path == '/session': self.json({'id': session})
        elif self.path == '/session/' + session + '/prompt_async':
            nonce = payload['parts'][0]['text']
            messages.append({'info': {'id': 'fixture-user', 'role': 'user', 'sessionID': session}, 'parts': [{'type': 'text', 'text': nonce}]})
            model(False)
            emit('session.status', {'status': {'type': 'busy'}})
            event_ask = json.loads(json.dumps(ask))
            if failure == 'approvalCorrelation': event_ask['tool']['callID'] = 'wrong-call'
            emit('permission.asked', event_ask)
            asks.append(ask)
            self.json({}, 204)
            threading.Timer(1.5, lambda: os._exit(0)).start()
        elif self.path == '/permission/fixture-permission/reply':
            assert payload == {'reply': 'once'}
            asks.clear()
            emit('permission.replied', {'requestID': 'fixture-permission', 'reply': 'once'})
            model(True)
            content = 'CONSUMED:' + nonce if failure != 'nativeConsumption' else 'accepted but not consumed'
            messages.append({'info': {'id': 'fixture-reply', 'role': 'assistant', 'sessionID': session, 'parentID': 'fixture-user', 'time': {'completed': 1}, 'finish': 'stop'}, 'parts': [{'type': 'text', 'text': content}]})
            if failure != 'idleEdge': emit('session.status', {'status': {'type': 'idle'}})
            if failure != 'lifecycle': emit('session.idle', {})
            self.json({})
        else: self.json({}, 404)
server = http.server.ThreadingHTTPServer(('127.0.0.1', int(sys.argv[sys.argv.index('--port') + 1])), Handler)
server.daemon_threads = True
server.serve_forever()
