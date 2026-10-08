"""Credential admission and current-person decision boundaries, without a key or API."""
import importlib.machinery
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
import http.client
import json
import socket
import ssl
import subprocess
import sys
import time
from content_persona import Persona
import claude_eval_broker as broker

loader=importlib.machinery.SourceFileLoader("content_eval",str(Path(__file__).resolve().parents[1]/"onboarding-content-eval"))
spec=importlib.util.spec_from_loader(loader.name,loader)
runner=importlib.util.module_from_spec(spec); loader.exec_module(runner)

class CredentialGuards(unittest.TestCase):
 def test_rejects_symlink_and_nonprivate_file_before_read(self):
  with tempfile.TemporaryDirectory() as directory:
   path=Path(directory)/"key"; path.write_text("test-key\n"); path.chmod(0o600)
   link=Path(directory)/"link"; link.symlink_to(path)
   with self.assertRaises(OSError): runner.guarded_key(link)
   path.chmod(0o644)
   with self.assertRaises(ValueError): runner.guarded_key(path)
 def test_rejects_internal_whitespace_and_oversize(self):
  with tempfile.TemporaryDirectory() as directory:
   path=Path(directory)/"key"; path.write_text("test key"); path.chmod(0o600)
   with self.assertRaises(ValueError): runner.guarded_key(path)
   path.write_text("x"*8193)
   with self.assertRaises(ValueError): runner.guarded_key(path)
 def test_scrubs_before_receipt_write(self):
  with tempfile.TemporaryDirectory() as directory:
   class Machine:
    ubuntu="24.04"; scenario="test"; name="fake"; uid=42420; memory_bytes=3*1024**3
    args=type("Args",(),{"backend":"docker"})()
    def run(self,*a,**k): return {"script":"test","stdout":"secret-test-value","stderr":"secret-test-value","exit":0,"seconds":0}
   run=runner.Evaluation(Machine(),Path(directory)/"out",Path("unused"),{"sha256":"test"},key="secret-test-value",variant=1)
   run.execute("receipt","test")
   for path in run.out.iterdir(): self.assertNotIn("secret-test-value",path.read_text())

class PersonBoundaries(unittest.TestCase):
 def fixture(self,name="tour",attempt=1,generation="current"):
  run={"generation":"current","requester":"person/ada","steps":[{"subject":"step-run/current/"+name,"generation":generation,"step":name,"attempt":1,"status":"waiting"}]}
  card={"id":"attention/first","state":"open","person_id":"person/ada","blocked":{"step_run_id":"step-run/current/"+name,"attempt":attempt},"request":{"type":"choice","answers":[{"id":"tour-seen"},{"id":"help"}]}}
  return card,run
 def test_old_generation_attempt_and_other_person_remain_unanswered(self):
  persona=Persona(1)
  for kw in ({"generation":"old"},{"attempt":2}):
   card,run=self.fixture(**kw); self.assertIsNone(persona.answer(card,run))
  card,run=self.fixture(); card["person_id"]="person/other"
  self.assertIsNone(persona.answer(card,run))
 def test_help_requires_a_fresh_request_before_tour_completion(self):
  persona=Persona(2); card,run=self.fixture()
  first=persona.answer(card,run); self.assertEqual(first["answer"],"help"); persona.answered(card,first)
  self.assertIsNone(persona.answer(card,run))
  card["id"]="attention/fresh"; self.assertEqual(persona.answer(card,run)["answer"],"tour-seen")
 def test_wrong_project_proposal_cannot_receive_acceptance(self):
  persona=Persona(1); card,run=self.fixture("your-project")
  card["request"]={"type":"decision","question":"Create claude in /home/ada/other","answers":[{"id":"accept"},{"id":"decline"}]}
  self.assertIsNone(persona.answer(card,run))
 def test_decline_then_fresh_accept_and_no_skip_of_required_lesson(self):
  persona=Persona(2); card,run=self.fixture("first-mission")
  card["request"]={"type":"decision","answers":[{"id":"accept"},{"id":"decline"}]}
  decision=persona.answer(card,run); self.assertEqual(decision["answer"],"decline"); persona.answered(card,decision)
  self.assertIsNone(persona.answer(card,run)); card["id"]="attention/revised"
  self.assertEqual(persona.answer(card,run)["answer"],"accept")
  card["request"]={"type":"choice","answers":[{"id":"enable"},{"id":"skip"}]}
  self.assertIsNone(persona.answer(card,run))

class PrivateBrokerBoundaries(unittest.TestCase):
 def test_beta_count_preserves_tool_caller_context_and_native_betas(self):
  payload=json.loads(Path(__file__).with_name("count-native-tool-result.json").read_text())
  payload.update(context_management={"edits":[]},cache_control={"type":"ephemeral"})
  path,body,headers=broker.count_request(payload,{"Anthropic-Beta":"claude-code-20250219"})
  decoded=json.loads(body)
  self.assertEqual(path,"/v1/messages/count_tokens?beta=true")
  self.assertIn("token-counting-2024-11-01",headers["anthropic-beta"])
  self.assertIn("claude-code-20250219",headers["anthropic-beta"])
  self.assertEqual(decoded["messages"],payload["messages"])
  self.assertEqual(decoded["context_management"],payload["context_management"])
  self.assertEqual(decoded["cache_control"],payload["cache_control"])
  self.assertNotIn("stream",decoded); self.assertNotIn("max_tokens",decoded)
  self.assertNotIn("_fixture",decoded)
 def exercise(self,status,input_tokens,tls=False,count_status=200):
  # Substitute the transport before starting the broker: no real provider calls.
  with tempfile.TemporaryDirectory() as directory:
   with socket.socket() as sock:
    sock.bind(("127.0.0.1",0)); port=sock.getsockname()[1]
   cert=Path(directory)/"cert.pem"; tls_key=Path(directory)/"tls-key.pem"
   if tls:
    subprocess.run(["openssl","req","-x509","-newkey","rsa:2048","-nodes","-days","1","-subj","/CN=api.anthropic.com","-addext","subjectAltName=DNS:api.anthropic.com","-keyout",str(tls_key),"-out",str(cert)],check=True,capture_output=True)
   program='''import sys,json,http.client
sys.path.insert(0,sys.argv[1]); import claude_eval_broker as broker
broker.PORT=int(sys.argv[2]); broker.SOCKET=sys.argv[3]
broker.CERT=sys.argv[6]; broker.TLS_KEY=sys.argv[7]
status=int(sys.argv[4]); tokens=int(sys.argv[5]); count_status=int(sys.argv[8])
class Response:
 def __init__(self,path):
  self.status=count_status if "count_tokens" in path else status
  self.body=json.dumps({"input_tokens":tokens} if "count_tokens" in path and count_status==200 else {"type":"error","error":{"type":"invalid_request_error" if count_status!=200 else "rate_limit_error" if status==429 else "authentication_error","message":"fixture-key count failure"}}).encode()
 def read(self): return self.body
class Connection:
 def __init__(self,*a,**k): pass
 def request(self,method,path,**kw): self.path=path
 def getresponse(self): return Response(self.path)
 def close(self): pass
http.client.HTTPSConnection=Connection
broker.main()
'''
   child=subprocess.Popen([sys.executable,"-c",program,str(Path(__file__).parent),str(port),str(Path(directory)/"key.sock"),str(status),str(input_tokens),str(cert),str(tls_key),str(count_status)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
   child.stdin.write(b"fixture-key\n"); child.stdin.close()
   def request(method,path,body=None):
    if tls and method=="POST":
     conn=http.client.HTTPSConnection("127.0.0.1",port,timeout=3,context=ssl.create_default_context(cafile=str(cert)))
     conn.set_tunnel("api.anthropic.com",443)
    else: conn=http.client.HTTPConnection("127.0.0.1",port,timeout=3)
    conn.request(method,path,body,headers={"x-api-key":"fixture-key","Content-Type":"application/json"})
    response=conn.getresponse(); result=response.status,response.read(); conn.close(); return result
   try:
    for _ in range(50):
     try: request("GET","/status"); break
     except OSError: time.sleep(.02)
    payload=json.dumps({"model":runner.MODEL,"max_tokens":100,"messages":[{"role":"user","content":"test"}]})
    code,body=request("POST","/v1/messages",payload)
    self.assertNotIn(b"fixture-key",body)
    first=json.loads(request("GET","/status")[1])
    stopped_code,stopped_body=request("POST","/v1/messages",payload)
    self.assertEqual(stopped_code,400)
    self.assertEqual(json.loads(stopped_body)["error"]["type"],"invalid_request_error")
    second=json.loads(request("GET","/status")[1])
    self.assertEqual(first["upstream_calls"],second["upstream_calls"])
    request("GET","/api/claude_cli/bootstrap")
    self.assertEqual(second["upstream_calls"],json.loads(request("GET","/status")[1])["upstream_calls"])
    return code,first
   finally:
    child.terminate(); child.wait(timeout=3); child.stdout.close(); child.stderr.close()
 def test_count_failure_records_sanitized_error_and_blocks_followup(self):
  code,state=self.exercise(200,100,count_status=400)
  self.assertEqual(code,400); self.assertEqual(state["stopped"],"token-count-error")
  self.assertEqual(state["upstream_calls"],1); self.assertEqual(state["message_calls"],0)
  error=state["requests"][0]["token_count_error"]
  self.assertEqual(error["type"],"invalid_request_error")
  self.assertEqual(error["message"],"[REDACTED] count failure")
 def test_first_authentication_error_blocks_every_later_upstream_call(self):
  code,state=self.exercise(401,100)
  self.assertEqual(code,401); self.assertEqual(state["stopped"],"authentication-or-rate-limit"); self.assertEqual(state["upstream_calls"],2)
 def test_first_rate_limit_blocks_every_later_upstream_call(self):
  code,state=self.exercise(429,100)
  self.assertEqual(code,429); self.assertEqual(state["stopped"],"authentication-or-rate-limit"); self.assertEqual(state["upstream_calls"],2)
 def test_budget_rejects_before_a_paid_message(self):
  code,state=self.exercise(401,1_000_000)
  self.assertEqual(code,400); self.assertEqual(state["stopped"],"evaluation-budget-or-call-cap"); self.assertEqual(state["upstream_calls"],1); self.assertEqual(state["message_calls"],0)
 def test_https_proxy_preserves_hostname_and_stops_after_first_auth_error(self):
  code,state=self.exercise(401,100,tls=True)
  self.assertEqual(code,401); self.assertEqual(state["stopped"],"authentication-or-rate-limit"); self.assertEqual(state["upstream_calls"],2)

if __name__=="__main__": unittest.main()
