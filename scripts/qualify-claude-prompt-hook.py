#!/usr/bin/env python3
import argparse,hashlib,json,os,socket,subprocess,tempfile,threading,time
from pathlib import Path
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
parser=argparse.ArgumentParser(description="Finite Claude permission-hook qualification, using only a loopback model and invented credentials.")
parser.add_argument('--claude',type=Path,required=True)
parser.add_argument('--answer',choices=['allow','deny'],required=True)
args=parser.parse_args()
assert hashlib.sha256(args.claude.read_bytes()).hexdigest()=='a967e7b1d8b4e47ee421d5433027880347952b0c0857abf880e2c942a4ec93b3', 'fixture requires the qualified public Claude 2.1.292 artifact'
command="printf 'fixture-%s' approved"
requests=[]
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_POST(self):
  value=json.loads(self.rfile.read(int(self.headers['Content-Length'])));requests.append(value)
  continuation=any(isinstance(m.get('content'),list) and any(c.get('type')=='tool_result' for c in m['content']) for m in value.get('messages',[]))
  msg={'id':'msg_fixture','type':'message','role':'assistant','model':'claude-sonnet-4-6','usage':{'input_tokens':1,'output_tokens':1},'stop_sequence':None,'content':[], 'stop_reason':'end_turn' if continuation else 'tool_use'}
  block={'type':'text','text':'fixture complete'} if continuation else {'type':'tool_use','id':'toolu_fixture','name':'Bash','input':{'command':command,'description':'Harmless isolated fixture'}}
  if value.get('stream'):
   events=[('message_start',{'type':'message_start','message':msg}),('content_block_start',{'type':'content_block_start','index':0,'content_block':dict(block,**({'input':{}} if not continuation else {}))})]
   if not continuation:events.append(('content_block_delta',{'type':'content_block_delta','index':0,'delta':{'type':'input_json_delta','partial_json':json.dumps(block['input'])}}))
   events += [('content_block_stop',{'type':'content_block_stop','index':0}),('message_delta',{'type':'message_delta','delta':{'stop_reason':msg['stop_reason'],'stop_sequence':None},'usage':{'output_tokens':1}}),('message_stop',{'type':'message_stop'})]
   body=''.join('event: '+e+'\ndata: '+json.dumps(v)+'\n\n' for e,v in events).encode();ctype='text/event-stream'
  else:msg['content']=[block];body=json.dumps(msg).encode();ctype='application/json'
  self.send_response(200);self.send_header('Content-Type',ctype);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
server=ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=server.serve_forever,daemon=True).start()
with tempfile.TemporaryDirectory(prefix='claude-native-fixture-') as temp:
 root=Path(temp);config=root/'config';config.mkdir();workspace=root/'workspace';workspace.mkdir();endpoint=root/'reply';listener=socket.socket(socket.AF_UNIX);listener.bind(str(endpoint));listener.listen(1);listener.settimeout(25)
 hook=root/'hook.py';hook.write_text('''import json,socket,sys
p=json.load(sys.stdin)
if sys.argv[1]=='pre':
 print(json.dumps({'hookSpecificOutput':{'hookEventName':'PreToolUse','permissionDecision':'ask','permissionDecisionReason':'Fixture person must choose'}}))
else:
 s=socket.socket(socket.AF_UNIX);s.connect(sys.argv[2]);s.sendall((json.dumps(p)+'\\n').encode());data=s.recv(4096);print(data.decode());s.close()
''')
 mcp=root/'mcp.py';mcp.write_text('''import json,sys,time
for line in sys.stdin:
 p=json.loads(line);method=p.get('method');result={}
 if 'id' not in p:continue
 if method=='initialize':result={'protocolVersion':'2024-11-05','capabilities':{'tools':{}},'serverInfo':{'name':'fixture','version':'1'}}
 elif method=='tools/list':result={'tools':[{'name':'permission_fixture','description':'Confined fixture permission host','inputSchema':{'type':'object','properties':{'tool_name':{'type':'string'},'input':{'type':'object'}},'additionalProperties':True}}]}
 elif method=='tools/call':
  time.sleep(20);result={'content':[{'type':'text','text':json.dumps({'behavior':'deny','message':'Fixture host response deadline elapsed'})}]}
 print(json.dumps({'jsonrpc':'2.0','id':p['id'],'result':result}),flush=True)
''')
 mcpconfig=root/'mcp.json';mcpconfig.write_text(json.dumps({'mcpServers':{'fixture':{'command':'python3','args':[str(mcp)]}}}))
 settings=root/'settings.json';settings.write_text(json.dumps({'hooks':{'PreToolUse':[{'matcher':'Bash','hooks':[{'type':'command','command':f'python3 {hook} pre','timeout':30}]}],'PermissionRequest':[{'matcher':'Bash','hooks':[{'type':'command','command':f'python3 {hook} answer {endpoint}','timeout':30}]}]}}))
 env={'PATH':os.environ['PATH'],'CLAUDE_CONFIG_DIR':str(config),'ANTHROPIC_API_KEY':'invented-fixture-key','ANTHROPIC_BASE_URL':f'http://127.0.0.1:{server.server_port}','CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC':'1','TERM':'dumb'}
 proc=subprocess.Popen([str(args.claude),'-p','--settings',str(settings),'--setting-sources','','--mcp-config',str(mcpconfig),'--strict-mcp-config','--permission-prompt-tool','mcp__fixture__permission_fixture','--permission-mode','default','--tools','Bash','--model','claude-sonnet-4-6','--max-turns','2','native fixture only'],cwd=workspace,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
 try:
  peer,_=listener.accept();payload=json.loads(peer.makefile('rb').readline());assert payload['tool_input']['command']==command
  peer.sendall((json.dumps({'hookSpecificOutput':{'hookEventName':'PermissionRequest','decision':{'behavior':args.answer}}})+'\n').encode());peer.close()
  out,err=proc.communicate(timeout=20)
  results=[c for r in requests for m in r.get('messages',[]) if isinstance(m.get('content'),list) for c in m['content'] if c.get('type')=='tool_result']
  assert proc.returncode==0
  assert results
  assert any(('fixture-approved' in str(r.get('content'))) == (args.answer=='allow') for r in results)
  print(json.dumps({'answer':args.answer,'version':'2.1.292','exit':proc.returncode,'hook_event':payload.get('hook_event_name'),'hook_keys':sorted(payload.keys()),'requests':len(requests),'tool_results':results,'output':out.decode()[:200],'stderr':err.decode()[:400]},indent=2))
 except Exception as error:
  proc.kill();out,err=proc.communicate();print(json.dumps({'error':str(error),'requests':len(requests),'tools':[[t.get('name') for t in r.get('tools',[])] for r in requests],'results':[c for r in requests for m in r.get('messages',[]) if isinstance(m.get('content'),list) for c in m['content'] if c.get('type')=='tool_result'],'output':out.decode()[:300],'stderr':err.decode()[:1000]},indent=2));raise
server.shutdown()
