#!/usr/bin/env python3
"""Disposable-guest private key helper broker and bounded Anthropic transport.

Read the key once from a private stdin pipe. Never put it in an environment,
file, log or result. Only the helper's private socket returns it to Claude.
"""
import http.client
import http.server
import json
import os
from pathlib import Path
import socket
import ssl
import threading
import time

MODEL="claude-haiku-4-5-20251001"
SOCKET="/home/ada/.local/state/onboarding-eval/key.sock"
PORT=18443
CERT="/home/ada/.local/state/onboarding-eval/proxy-cert.pem"
TLS_KEY="/home/ada/.local/state/onboarding-eval/proxy-tls-key.pem"

def count_request(payload,headers):
 """Match the official beta countTokens API, including native tool-result context."""
 fields={"model","messages","cache_control","compaction","context_management","mcp_servers","output_config","output_format","speed","system","thinking","tool_choice","tools"}
 body={k:v for k,v in payload.items() if k in fields}
 beta=next((v for k,v in headers.items() if k.lower()=="anthropic-beta"),"")
 headers={k:v for k,v in headers.items() if k.lower()!="anthropic-beta"}
 headers["anthropic-beta"]=",".join(dict.fromkeys([*(x.strip() for x in beta.split(",") if x.strip()),"token-counting-2024-11-01"]))
 return "/v1/messages/count_tokens?beta=true",json.dumps(body).encode(),headers

def main():
 key=__import__('sys').stdin.readline().strip()
 assert key and not any(c.isspace() for c in key)
 deadline=time.monotonic()+600
 lock=threading.Lock()
 state={"model":MODEL,"message_calls":0,"http_calls":0,"upstream_calls":0,"usage":{},"usage_cost_usd":0.0,
        "committed_budget_usd":0.0,"budget_usd":2.0,"stopped":None,"requests":[]}
 def stop(reason):
  with lock:
   if state["stopped"] is None: state["stopped"]=reason
 def request_api(path,body,headers,method="POST"):
  with lock: state["upstream_calls"]+=1
  conn=http.client.HTTPSConnection("api.anthropic.com",timeout=60)
  headers={k:v for k,v in headers.items() if k.lower() not in ("host","connection","content-length","accept-encoding")}
  headers["Accept-Encoding"]="identity"
  conn.request(method,path,body=body,headers=headers)
  return conn,conn.getresponse()
 def auth_error(status,data):
  kind=(data.get("error") or {}).get("type") if isinstance(data,dict) else None
  if status in (401,403,429) or kind in ("authentication_error","permission_error","rate_limit_error"):
   stop("authentication-or-rate-limit"); return True
  return False
 def helper_server():
  path=Path(SOCKET); path.parent.mkdir(parents=True,exist_ok=True)
  server=socket.socket(socket.AF_UNIX); server.bind(SOCKET); os.chmod(SOCKET,0o600); server.listen()
  while time.monotonic()<deadline:
   conn,_=server.accept()
   with conn:
    if conn.recv(16)==b"key\n": conn.sendall(key.encode()+b"\n")
  server.close()
 class Handler(http.server.BaseHTTPRequestHandler):
  def log_message(self,*args): pass
  def metadata(self,method,body=None):
   with lock: halted=state["stopped"]
   if halted: return self.respond(400,{"type":"error","error":{"type":"invalid_request_error","message":"Evaluation fixture stopped; no upstream retry is permitted."}})
   try:
    conn,response=request_api(self.path,body,dict(self.headers),method=method)
    raw=response.read().replace(key.encode(),b"[REDACTED]"); status=response.status; conn.close()
    try: data=json.loads(raw)
    except ValueError: data=None
    auth_error(status,data)
    with lock: state["requests"].append({"metadata":True,"path":self.path.split("?")[0],"status":status})
    self.send_response(status); self.send_header("Content-Type","application/json"); self.send_header("Content-Length",str(len(raw))); self.end_headers(); self.wfile.write(raw)
   except Exception as error: stop("transport-"+type(error).__name__)
  def respond(self,status,data):
   content=json.dumps(data).encode().replace(key.encode(),b"[REDACTED]"); self.send_response(status)
   self.send_header("Content-Type","application/json"); self.send_header("Content-Length",str(len(content))); self.end_headers(); self.wfile.write(content)
  def do_GET(self):
   if time.monotonic()>=deadline: stop("wall-clock-limit")
   if self.path=="/safety":
    processes=[]; files=[]
    for directory in Path("/proc").iterdir():
     if not directory.name.isdecimal(): continue
     try:
      if directory.stat().st_uid!=os.getuid(): continue
      if key.encode() in (directory/"environ").read_bytes(): processes.append(int(directory.name))
     except OSError: pass
    for path in Path.home().rglob("*"):
     try:
      if path.is_file() and not path.is_symlink() and path.stat().st_size<=10_000_000 and key.encode() in path.read_bytes():
       files.append(str(path.relative_to(Path.home())))
     except OSError: pass
    return self.respond(200,{"credential_environment_processes":processes,"credential_files":files,"checked_uid":os.getuid()})
   if self.path!="/status":
    if not isinstance(self.connection,ssl.SSLSocket): return self.respond(404,{"error":"not found"})
    return self.metadata("GET")
   with lock: snapshot=json.loads(json.dumps(state))
   self.respond(200,snapshot)
  def do_CONNECT(self):
   if self.path!="api.anthropic.com:443":
    # Other native metadata/telemetry endpoints stay opaque; no body is inspected.
    host,port=self.path.rsplit(":",1)
    if int(port)!=443: return self.respond(403,{"error":"HTTPS only"})
    import select
    upstream=socket.create_connection((host,443),timeout=15)
    self.send_response(200,"Connection Established"); self.end_headers(); self.wfile.flush(); self.close_connection=True
    self.connection.setblocking(False); upstream.setblocking(False)
    try:
     until=time.monotonic()+60
     while time.monotonic()<until:
      ready,_,_=select.select([self.connection,upstream],[],[],1)
      for source in ready:
       data=source.recv(65536)
       if not data: return
       (upstream if source is self.connection else self.connection).sendall(data)
    finally: upstream.close()
    return
   context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
   context.load_cert_chain(CERT,TLS_KEY)
   self.send_response(200,"Connection Established"); self.end_headers(); self.wfile.flush(); self.close_connection=True
   with context.wrap_socket(self.connection,server_side=True) as secured:
    Handler(secured,self.client_address,self.server)
  def do_POST(self):
   if time.monotonic()>=deadline: stop("wall-clock-limit")
   body=self.rfile.read(int(self.headers.get("Content-Length","0")))
   with lock:
    state["http_calls"]+=1
    halted=state["stopped"]
   if halted or time.monotonic()>=deadline:
    return self.respond(400,{"type":"error","error":{"type":"invalid_request_error","message":"Evaluation fixture stopped; no upstream retry is permitted."}})
   if not self.path.startswith("/v1/messages"):
    if not isinstance(self.connection,ssl.SSLSocket): return self.respond(404,{"error":"native HTTPS metadata only"})
    return self.metadata("POST",body)
   if self.headers.get("x-api-key")!=key:
    stop("credential-helper-mismatch")
    return self.respond(401,{"type":"error","error":{"type":"authentication_error","message":"Private credential helper mismatch."}})
   try: payload=json.loads(body)
   except ValueError: return self.respond(400,{"error":"invalid JSON"})
   if not self.path.startswith("/v1/messages"):
    return self.respond(404,{"error":"only native message and token-count routes are allowed"})
   if payload.get("model")!=MODEL:
    stop("unexpected-model")
    return self.respond(400,{"error":"evaluation model is pinned"})
   reserve=0.0; usage={}; record={"path":self.path,"model":MODEL}
   try:
    if "/count_tokens" not in self.path:
     payload["max_tokens"]=min(int(payload.get("max_tokens",2048)),2048)
     path,body_for_count,headers_for_count=count_request(payload,dict(self.headers))
     conn,count=request_api(path,body_for_count,headers_for_count)
     raw=count.read(); count_status=count.status; conn.close()
     try: count_data=json.loads(raw)
     except ValueError: count_data={}
     record["token_count_status"]=count_status
     if count_status!=200:
      error=count_data.get("error") or {}
      record["token_count_error"]={"type":error.get("type"),"message":str(error.get("message","")).replace(key,"[REDACTED]")[:512]}
     if auth_error(count_status,count_data): return self.respond(count_status,count_data)
     if count_status!=200:
      stop("token-count-error"); return self.respond(count_status,count_data)
     # Worst-case 1h cache creation at $2/M input plus a 10% geographic uplift.
     # Output uses $5/M. Usage below reports actual native usage separately.
     reserve=(count_data["input_tokens"]*2.2+payload["max_tokens"]*5.5)/1_000_000
     with lock:
      if state["message_calls"]>=60 or state["committed_budget_usd"]+reserve>state["budget_usd"]:
       state["stopped"]="evaluation-budget-or-call-cap"
       return self.respond(400,{"error":"evaluation budget or call cap reached"})
      state["message_calls"]+=1; state["committed_budget_usd"]+=reserve
     body=json.dumps(payload).encode()
    conn,response=request_api(self.path,body,dict(self.headers)); record["status"]=response.status
    if response.status!=200:
     raw=response.read()
     try: data=json.loads(raw)
     except ValueError: data={"error":"provider returned non-JSON error"}
     auth_error(response.status,data)
     self.respond(response.status,data); conn.close(); return
    self.send_response(response.status)
    self.send_header("Content-Type",response.getheader("Content-Type","application/json"))
    self.send_header("Connection","close"); self.end_headers(); self.close_connection=True
    if payload.get("stream"):
     while True:
      line=response.readline()
      if not line: break
      if line.startswith(b"data: "):
       try:
        event=json.loads(line[6:]); actual=(event.get("message") or {}).get("usage") or event.get("usage") or {}
        usage.update({k:v for k,v in actual.items() if isinstance(v,(int,dict))})
        if event.get("type")=="error": auth_error(200,event)
       except ValueError: pass
      self.wfile.write(line.replace(key.encode(),b"[REDACTED]")); self.wfile.flush()
    else:
     raw=response.read()
     try:
      result=json.loads(raw); usage=result.get("usage") or {}
     except ValueError: pass
     self.wfile.write(raw.replace(key.encode(),b"[REDACTED]"))
    conn.close()
   except Exception as error:
    stop("transport-"+type(error).__name__)
    # Never serialize headers, the key, or a request/response payload in errors.
   finally:
    creation=usage.get("cache_creation") or {}
    hour=creation.get("ephemeral_1h_input_tokens",0)
    short=creation.get("ephemeral_5m_input_tokens",usage.get("cache_creation_input_tokens",0)-hour)
    cost=(usage.get("input_tokens",0)+short*1.25+hour*2+
          usage.get("cache_read_input_tokens",0)*0.1+usage.get("output_tokens",0)*5)/1_000_000
    if payload.get("inference_geo")=="us": cost*=1.1
    record["usage"]=usage; record["usage_cost_usd"]=cost
    with lock:
     for k,v in usage.items():
      if isinstance(v,int): state["usage"][k]=state["usage"].get(k,0)+v
     state["usage_cost_usd"]+=cost
     if usage: state["committed_budget_usd"]+=cost-reserve
     state["requests"].append(record)
 threading.Thread(target=helper_server,daemon=True).start()
 http.server.ThreadingHTTPServer(("127.0.0.1",PORT),Handler).serve_forever()

if __name__=="__main__": main()
