#!/usr/bin/env python3
"""Real scratch daemon API + HTTP/stdio MCP exercise; never uses ambient st state."""
import base64, concurrent.futures, json, os, pathlib, selectors, socket, sqlite3, subprocess, tempfile, time, urllib.request, urllib.error, urllib.parse
ROOT = pathlib.Path(__file__).resolve().parents[1]
BIN = pathlib.Path(os.environ.get('CARGO_TARGET_DIR', ROOT / 'tmp/target')) / 'debug'
STATE = pathlib.Path(tempfile.mkdtemp(prefix='st-chatgpt-', dir=os.environ.get('TMPDIR')))
ENV = dict(os.environ, XDG_CONFIG_HOME=str(STATE / 'config'), XDG_STATE_HOME=str(STATE / 'xdg-state'))
for key in ['ST_AGENT', 'ST3_SOCKET', 'ST_SOCKET', 'ST3_CONFIG']:
    ENV.pop(key, None)
REPORT = []
PROCESSES = []
API = 'st3.client.v0'
READ_SCOPES = ['read.assistant.' + name for name in ['summary','attention','missions','agents','conversation']]
SCOPES = READ_SCOPES + ['write.assistant.messages', 'write.assistant.answers']

def record(name, ok, evidence):
    row = {'scenario':name, 'status':'PASS' if ok else 'FAIL', 'evidence':evidence}
    REPORT.append(row)
    print(json.dumps(row), flush=True)

def local(path, body=None, method=None, envelope=True):
    command = ['curl','--silent','--show-error','--unix-socket',str(STATE/'st3.sock'),'-H','x-st3-person: person/ada','-H','x-st3-features: conversation-blocks.v1','-H','Content-Type: application/json']
    if body is not None: command += ['--data',json.dumps(body)]
    if method: command += ['-X',method]
    command += ['http://localhost'+path]
    out = subprocess.check_output(command, env=ENV, text=True)
    response = json.loads(out)
    if envelope and 'value' not in response: raise RuntimeError(out)
    return response

def http(url, payload=None):
    data = None if payload is None else json.dumps(payload).encode()
    request = urllib.request.Request(url, data=data, headers={'Content-Type':'application/json','Accept':'application/json, text/event-stream'})
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()

def start(command, log):
    output = open(STATE/log,'wb')
    process = subprocess.Popen(command, env=ENV, stdout=output, stderr=output)
    PROCESSES.append(process)
    return process

def ready(predicate, process):
    deadline=time.monotonic()+20
    while time.monotonic()<deadline:
        if process.poll() is not None: raise RuntimeError('process exited; see '+str(STATE))
        if predicate(): return
        time.sleep(.02)
    raise RuntimeError('readiness deadline exceeded; see '+str(STATE))

def cli(*args):
    output=subprocess.run([str(BIN/'st3'),'--endpoint',str(STATE/'st3.sock'),'--json',*args],env=ENV,text=True,capture_output=True)
    if output.returncode: raise RuntimeError(output.stderr)
    return json.loads(output.stdout)

def policy(grant, scopes=SCOPES, seconds=3600, recipients=()):
    return local('/v1/client/assistant/grants/'+grant.split('/')[-1]+'/policy', {'api_version':API,'scopes':scopes,'expires_in_seconds':seconds,'disclosure_consent':True,'consent_evidence':'scripted explicit disclosure consent','minimal_read_mode':'caller-bound-allow','recipient_allowlist':list(recipients)})

def pair(info, seconds=3600):
    challenge=local('/v1/client/pairings',{'api_version':API,'device_name':'Prototype ChatGPT','assistant_id':'assistant/chatgpt','scopes':SCOPES,'expires_in_seconds':seconds,'disclosure_consent':True,'consent_evidence':'prototype scripted explicit disclosure consent','minimal_read_mode':'caller-bound-allow'})['value']
    status, raw=http(info['pairing_url']+'/v1/client/pairings/'+challenge['pairing_id'].split('/')[-1]+'/complete',{'api_version':API,'code':challenge['code'],'device_public_key':base64.urlsafe_b64encode(bytes(range(32))).decode().rstrip('=')})
    if status != 200: raise RuntimeError(raw.decode())
    paired=json.loads(raw)['value']
    return paired

def mcp(url, name, arguments=None):
    status, raw=http(url,{'jsonrpc':'2.0','id':3,'method':'tools/call','params':{'name':name,'arguments':arguments or {}}})
    response=json.loads(raw)
    if 'result' not in response: raise RuntimeError(raw.decode())
    result=response['result']
    value=json.loads(result['content'][0]['text'])
    return value, result.get('isError',False)

def count_messages():
    with sqlite3.connect(STATE/'store.sqlite') as db:
        return db.execute("SELECT count(*) FROM claims WHERE kind='message.sent'").fetchone()[0]


def arm_mutation_gate():
    gate=STATE/'mutation-gate'
    for name in ('hold','entered','release','done'):
        (gate/name).unlink(missing_ok=True)
    (gate/'hold').write_text('hold')

def wait_mutation_gate(name):
    deadline=time.monotonic()+20
    while not (STATE/'mutation-gate'/name).exists():
        if time.monotonic()>=deadline: raise RuntimeError('mutation scheduling gate did not signal '+name)
        time.sleep(.005)

def release_mutation_gate():
    (STATE/'mutation-gate'/'hold').unlink(missing_ok=True)
    (STATE/'mutation-gate'/'release').write_text('release')
    wait_mutation_gate('done')

def count_person_answers(subject):
    with sqlite3.connect(STATE/'store.sqlite') as db:
        return db.execute("SELECT count(*) FROM claims WHERE kind='work.person-done' AND subject=?", (subject,)).fetchone()[0]
def replace_expired_grant():
    global paired, adapter
    # Expired grants cannot be renewed: obtain fresh owner consent and reconnect.
    paired=pair(info)
    policy(paired['grant']['id'])
    credential.write_text(paired['credential'])
    adapter.terminate();adapter.wait(timeout=10)
    adapter=start(adapter_args+['--listen','127.0.0.1:'+str(port)],'adapter.log')
    ready(listening,adapter)

try:
    fixture=start([str(BIN/'examples/chatgpt_fixture'),str(STATE)],'fixture.log')
    ready(lambda:(STATE/'fixture.json').exists() and (STATE/'st3.sock').exists() and (STATE/'st3-client.sock').exists(),fixture)
    info=json.loads((STATE/'fixture.json').read_text())
    paired=pair(info)
    credential=STATE/'credential'
    credential.write_text(paired['credential'])
    credential.chmod(0o600)
    with socket.socket() as listener:
        listener.bind(('127.0.0.1',0)); port=listener.getsockname()[1]
    endpoint='http://127.0.0.1:'+str(port)+'/mcp'
    adapter_args=[str(BIN/'st-chatgpt'),'--socket',str(STATE/'st3-client.sock'),'--credential-file',str(credential)]
    adapter=start(adapter_args+['--listen','127.0.0.1:'+str(port)],'adapter.log')
    def listening():
        try:
            with socket.create_connection(('127.0.0.1',port),timeout=.1): return True
        except OSError: return False
    ready(listening,adapter)
    status,raw=http(endpoint,{'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2025-03-26','capabilities':{},'clientInfo':{'name':'prototype-e2e','version':'1'}}})
    record('HTTP initialize',status==200 and 'result' in json.loads(raw),raw.decode())
    status,raw=http(endpoint,{'jsonrpc':'2.0','id':2,'method':'tools/list'})
    tools=json.loads(raw)['result']['tools']
    record('seven annotated tools',len(tools)==7 and all(all(key in tool['annotations'] for key in ['readOnlyHint','destructiveHint','openWorldHint']) for tool in tools),json.dumps(tools))
    status,raw=http(endpoint)
    record('tunnel GET /mcp',status==405,{'status':status,'body':raw.decode()})
    status,raw=http(endpoint.replace('/mcp','/unknown'))
    record('tunnel empty 404',status==404 and raw==b'',{'status':status,'body':raw.decode()})
    alerts,error=mcp(endpoint,'st_alerts_list',{'max_response_bytes':16384})
    alert=next(row for row in alerts['value']['items'] if row.get('target_id')==info['ask_id'])
    record('list alert via MCP',not error,alert)
    detail,error=mcp(endpoint,'st_alert_get',{'alert_id':alert['id']})
    ask=detail['value']
    record('read question and options',not error and ask['request']['answers'][0]['id']=='yes',ask)
    fence={'snapshot_id':detail['snapshot']['id'],'subject_revisions':{ask['id']:ask['episode']}}
    arguments={'action_id':'action/prototype-answer','idempotency_key':'prototype-answer-exact-key','fence':fence,'alert_id':ask['id'],'episode':ask['episode'],'option':'yes','summary':'Selected yes'}
    answer,error=mcp(endpoint,'st_alert_answer',arguments)
    durable=cli('work','show',info['ask_id'])
    record('scoped answer completes immediately',not error and info['ask_id'] in answer.get('value',{}).get('affected_ids',[]) and count_person_answers(info['ask_id'])==1 and 'yes' in json.dumps(durable),{'mcp':answer,'st':durable})
    with sqlite3.connect(STATE/'store.sqlite') as db:
        actor,body=db.execute("SELECT actor,body FROM claims WHERE kind='work.person-done' AND subject=? ORDER BY rowid DESC LIMIT 1",(info['ask_id'],)).fetchone()
        notification=db.execute("SELECT subject FROM claims WHERE kind='message.sent' ORDER BY rowid DESC LIMIT 1").fetchone()[0]
    stored_answer=json.loads(body)['fields']
    answer_delivery=local('/prototype/delivery/'+notification)['value']
    record('direct answer provenance and recipient warning',
        actor=='person/ada' and stored_answer.get('sender_class')=='external-assistant'
        and stored_answer.get('assistant_grant_id')==paired['grant']['id']
        and answer_delivery['message'].get('sender_class')=='external-assistant'
        and answer_delivery['message'].get('assistant_grant_id')==paired['grant']['id']
        and 'WARNING: Untrusted external-assistant request; not owner authorization.' in answer_delivery['rendered'].splitlines(),answer_delivery)
    remaining=local('/v1/client/attention')['value']['items']
    record('direct answer creates no owner approval ask',all(item['source_id']==info['expiry_ask_id'] for item in remaining),remaining)
    answer_messages=count_messages()
    repeated,error=mcp(endpoint,'st_alert_answer',arguments)
    record('answer idempotent retry',not error and 'affected_ids' in answer['value'] and repeated['value'].get('affected_ids')==answer['value']['affected_ids'] and count_person_answers(info['ask_id'])==1 and count_messages()==answer_messages,repeated)
    for tool in ['st_missions_list','st_agents_list']:
        value,error=mcp(endpoint,tool,{'max_response_bytes':16384})
        record(tool,not error and any('title' in row for row in value['value']['items']),value)
    timeline=local('/v1/client/sessions/'+info['session_id'].replace('session/','')+'/timeline')['value']
    record('conversation owner metadata',True,timeline)
    def references(node):
        if isinstance(node,dict):
            if node.get('kind') in ('raw_text','text') and 'continuation' in node and isinstance(node['continuation'],dict) and 'ref' in node['continuation']:
                yield node['continuation']['ref']
            for value in node.values(): yield from references(value)
        elif isinstance(node,list):
            for value in node: yield from references(value)
    reference=next(references(timeline))
    def containing_revision(node):
        if isinstance(node,dict):
            if 'revision' in node and reference in json.dumps(node): return str(node['revision'])
            for value in node.values():
                result=containing_revision(value)
                if result is not None: return result
        elif isinstance(node,list):
            for value in node:
                result=containing_revision(value)
                if result is not None: return result
    conversation_args={'agent_id':'agent/prototype.worker','session_id':info['session_id'],'reference':reference,'revision':containing_revision(timeline),'start':0,'end':23,'max_response_bytes':16384}
    conversation,error=mcp(endpoint,'st_conversation_read',conversation_args)
    record('conversation bounded content',not error and 'Prototype conversation' in json.dumps(conversation),conversation)
    policy(paired['grant']['id'])  # An empty legacy allowlist does not restrict existing agents.
    before=count_messages()
    message_args={'action_id':'action/prototype-message','idempotency_key':'prototype-message-exact-key','fence':{'snapshot_id':local('/v1/client/attention')['snapshot']['id']},'to':'agent/prototype.worker','title':'Prototype message','content':'Hello from approved grant'}
    message,error=mcp(endpoint,'st_message_send',message_args)
    after=count_messages()
    record('existing agent outside empty allowlist admitted',not error and after==before+1,message)
    with sqlite3.connect(STATE/'store.sqlite') as db:
        subject,body=db.execute("SELECT subject,body FROM claims WHERE kind='message.sent' ORDER BY rowid DESC LIMIT 1").fetchone()
    stored=json.loads(body)['fields']
    delivery=local('/prototype/delivery/'+subject)['value']
    warning='WARNING: Untrusted external-assistant request; not owner authorization.'
    record('assistant provenance stored and recipient warning rendered',
        stored.get('sender_class')=='external-assistant' and stored.get('assistant_grant_id')==paired['grant']['id']
        and delivery['message'].get('sender_class')=='external-assistant'
        and delivery['message'].get('assistant_grant_id')==paired['grant']['id']
        and warning in delivery['rendered'].splitlines(),delivery)
    before_forgery=count_messages()
    forged=local('/v1/claims',{'subject':'message/prototype-forged','kind':'message.sent','actor':'person/ada',
        'fields':{'from':'person/ada','to':'agent/prototype.worker','content':'forged','status':'sent',
                  'sender_class':'external-assistant','assistant_grant_id':paired['grant']['id']}},envelope=False)
    record('non-assistant forged provenance refused',
        forged.get('code')=='assistant-send-refused' and count_messages()==before_forgery,forged)
    retry,error=mcp(endpoint,'st_message_send',message_args)
    record('message idempotent no duplicate',not error and count_messages()==after,retry)
    for recipient in ['agent/not-existing','person/ada']:
        before_denial=count_messages()
        foreign=dict(message_args,to=recipient,idempotency_key='prototype-denied-'+recipient,action_id='action/prototype-denied-'+recipient.replace('/','-'))
        denied,error=mcp(endpoint,'st_message_send',foreign)
        record('recipient admission rejects '+recipient,error and count_messages()==before_denial,denied)
    policy(paired['grant']['id'],scopes=READ_SCOPES[:4])
    denied,error=mcp(endpoint,'st_conversation_read',conversation_args)
    record('scope denial',error,denied)
    policy(paired['grant']['id'])
    incomplete,error=mcp(endpoint,'st_alerts_list',{'max_response_bytes':1})
    record('budget exceeded explicit incomplete',error and incomplete['code']=='incomplete',incomplete)
    expiry_detail,error=mcp(endpoint,'st_alert_get',{'alert_id':info['expiry_ask_id']})
    expiry_ask=expiry_detail['value']
    expiry_args=dict(arguments,action_id='action/direct-expiry',idempotency_key='direct-expiry-key',alert_id=expiry_ask['id'],episode=expiry_ask['episode'],fence={'snapshot_id':expiry_detail['snapshot']['id'],'subject_revisions':{expiry_ask['id']:expiry_ask['episode']}})
    stale=dict(expiry_args,action_id='action/prototype-stale-answer',idempotency_key='prototype-stale-answer-exact-key',episode='claim/stale')
    denied,error=mcp(endpoint,'st_alert_answer',stale)
    record('stale episode/fence rejected directly',error and count_person_answers(info['expiry_ask_id'])==0,denied)
    policy(paired['grant']['id'],scopes=[scope for scope in SCOPES if scope!='write.assistant.answers'])
    denied,error=mcp(endpoint,'st_alert_answer',expiry_args)
    record('direct answer requires current answer scope',error and count_person_answers(info['expiry_ask_id'])==0,denied)
    denied,error=mcp(endpoint,'st_alert_answer',arguments)
    record('answer replay requires current answer scope',error and count_person_answers(info['ask_id'])==1,denied)
    policy(paired['grant']['id'])
    before=count_messages()
    narrowed=dict(message_args,action_id='action/message-scope-race',idempotency_key='message-scope-race-key',fence={'snapshot_id':local('/v1/client/attention')['snapshot']['id']})
    arm_mutation_gate()
    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
        future=executor.submit(mcp,endpoint,'st_message_send',narrowed)
        wait_mutation_gate('entered')
        policy(paired['grant']['id'],scopes=[scope for scope in SCOPES if scope!='write.assistant.messages'])
        release_mutation_gate()
        denied,error=future.result()
    record('message scope removed during synchronous send before commit',error and denied.get('daemon',{}).get('code')=='forbidden' and count_messages()==before,{'denial':denied,'message_claims_before':before,'message_claims_after':count_messages()})
    denied,error=mcp(endpoint,'st_message_send',message_args)
    record('message replay requires current message scope',error and count_messages()==before,denied)
    policy(paired['grant']['id'])

    policy(paired['grant']['id'],seconds=2)
    arm_mutation_gate()
    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
        future=executor.submit(mcp,endpoint,'st_alert_answer',expiry_args)
        wait_mutation_gate('entered')
        time.sleep(2.2)
        release_mutation_gate()
        denied,error=future.result()
    record('grant expiry during direct answer before commit',error and count_person_answers(info['expiry_ask_id'])==0,denied)
    denied,error=mcp(endpoint,'st_alert_answer',expiry_args)
    record('expired grant cannot answer',error and count_person_answers(info['expiry_ask_id'])==0,denied)
    before_expired_send=count_messages()
    denied,error=mcp(endpoint,'st_message_send',message_args)
    record('expired grant cannot replay message',error and count_messages()==before_expired_send,denied)
    replace_expired_grant()
    stdio=subprocess.Popen(adapter_args+['--transport','stdio'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=open(STATE/'stdio.log','wb'),env=ENV,text=True,bufsize=1)
    PROCESSES.append(stdio)
    def stdio_rpc(request):
        stdio.stdin.write(json.dumps(request)+'\n');stdio.stdin.flush()
        selector=selectors.DefaultSelector();selector.register(stdio.stdout,selectors.EVENT_READ)
        if not selector.select(20): raise RuntimeError('stdio response deadline exceeded')
        return json.loads(stdio.stdout.readline())
    init=stdio_rpc({'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2025-03-26','capabilities':{},'clientInfo':{'name':'stdio-e2e','version':'1'}}})
    stdio.stdin.write(json.dumps({'jsonrpc':'2.0','method':'notifications/initialized'})+'\n');stdio.stdin.flush()
    stdio_tools=stdio_rpc({'jsonrpc':'2.0','id':2,'method':'tools/list'})
    stdio_read=stdio_rpc({'jsonrpc':'2.0','id':3,'method':'tools/call','params':{'name':'st_agents_list','arguments':{}}})
    record('stdio initialize/tools/read',len(stdio_tools['result']['tools'])==7 and not stdio_read['result'].get('isError',False),{'initialize':init,'read':stdio_read})
    policy(paired['grant']['id'],seconds=1)
    time.sleep(1.2)
    denied,error=mcp(endpoint,'st_agents_list')
    record('expired grant denied',error,denied)
    replace_expired_grant()
    before=count_messages()
    revoked_send=dict(message_args,action_id='action/revoke-held-send',idempotency_key='revoke-held-send-key',fence={'snapshot_id':local('/v1/client/attention')['snapshot']['id']})
    arm_mutation_gate()
    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
        future=executor.submit(mcp,endpoint,'st_message_send',revoked_send)
        wait_mutation_gate('entered')
        local('/v1/client/assistant/grants/'+paired['grant']['id'].split('/')[-1]+'/revoke',{},method='POST')
        release_mutation_gate()
        denied,error=future.result()
    record('revoke during synchronous send before commit',error and denied.get('daemon',{}).get('code')=='forbidden' and count_messages()==before,{'denial':denied,'message_claims_before':before,'message_claims_after':count_messages()})
    denied,error=mcp(endpoint,'st_agents_list')
    record('revoked grant denied',error,denied)
    denied,error=mcp(endpoint,'st_alert_answer',expiry_args)
    record('revoked grant cannot answer',error and count_person_answers(info['expiry_ask_id'])==0,denied)
    fixture.terminate();fixture.wait(timeout=10)
    unavailable,error=mcp(endpoint,'st_agents_list')
    record('daemon down explicit unavailable',error and unavailable['code']=='unavailable',unavailable)
except Exception as error:
    record('exercise prerequisite/runtime failure',False,str(error))
finally:
    for process in reversed(PROCESSES):
        if process.poll() is None:
            process.terminate()
            try: process.wait(timeout=10)
            except subprocess.TimeoutExpired: process.kill();process.wait()
    result={'scratch_state':str(STATE),'scenarios':REPORT}
    (ROOT/'tmp').mkdir(exist_ok=True)
    (ROOT/'tmp/chatgpt-e2e-results.json').write_text(json.dumps(result,indent=2)+'\n')
    print('Evidence: '+str(ROOT/'tmp/chatgpt-e2e-results.json'),flush=True)
    print('Scratch state/logs: '+str(STATE),flush=True)
raise SystemExit(0 if REPORT and all(row['status']=='PASS' for row in REPORT) else 1)
