#!/usr/bin/env python3
"""Complete real native welcome choices without a model prompt or auth value."""
import fcntl,json,os,pty,re,select,signal,struct,subprocess,termios,time
from pathlib import Path

def main():
 env=dict(os.environ,TERM="xterm-256color")
 for name in ("ST_AGENT","ST3_SUBJECT","ST3_INCARNATION","ST3_ENDPOINT"):
  env.pop(name,None)
 master,slave=pty.openpty()
 fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack("HHHH",40,140,0,0))
 proc=subprocess.Popen(["claude","--dangerously-skip-permissions"],stdin=slave,stdout=slave,stderr=slave,env=env,cwd="/home/ada/garden-project",start_new_session=True)
 os.close(slave); raw=bytearray(); actions=[]; handled=set(); ready=False; selected_at={}; moves={}; completed=False
 until=time.monotonic()+90
 try:
  while time.monotonic()<until and proc.poll() is None:
   if select.select([master],[],[],0.2)[0]:
    try: chunk=os.read(master,65536)
    except OSError: break
    raw.extend(chunk)
    if b"\x1b[c" in chunk: os.write(master,b"\x1b[?1;2c")
   screen=re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07]*(?:\x07|\x1b\\)","",raw.decode(errors="replace"))
   compact=re.sub(r"\s+","",screen)
   for label,needle,answer in (("theme","Choose the text style",b"\r"),("security","Security notes:",b"\r"),("trust","Yes, I trust this folder",b"\r"),("permissions","WARNING: Claude Code running in Bypass Permissions mode",b"\r"),("api-consent","Yes, use this API key",b"\r")):
    if label=="security" and "PressEntertocontinue" not in compact: continue
    if label not in handled and re.sub(r"\s+","",needle) in compact:
     if label in ("trust","permissions"):
      yes="Yes,Itrustthisfolder" if label=="trust" else "Yes,Iaccept"
      if yes not in compact or "Entertoconfirm" not in compact: continue
      selections=re.findall(r"❯(?:No,exit|"+yes+r")",compact)
      selected=selections[-1] if selections else ""
      if selected=="❯No,exit":
       if moves.get(label,0)<3 and (label not in selected_at or time.monotonic()-selected_at[label]>1):
        os.write(master,b"\x1b[B"); selected_at[label]=time.monotonic(); moves[label]=moves.get(label,0)+1
        actions.append({"choice":label+"-selection","native_text":"No, exit selected; Yes below","input":"Down"})
       continue
      if selected!="❯"+yes or label in selected_at and time.monotonic()-selected_at[label]<.6: continue
     os.write(master,answer); handled.add(label); actions.append({"choice":label,"native_text":needle,"input":"Enter"})
   # Observe only state written by native Claude; never fabricate first-run markers.
   completed=False
   for p in (Path.home()/".claude.json",Path.home()/".claude/.claude.json"):
    try: completed|=json.loads(p.read_text()).get("hasCompletedOnboarding") is True
    except (OSError,ValueError): pass
   if completed and "Haiku4.5" in compact and "APIUsageBilling" in compact and "❯\u00a0" in screen:
    ready=True; break
 finally:
  if proc.poll() is None:
   os.killpg(proc.pid,signal.SIGTERM)
   try: proc.wait(timeout=3)
   except subprocess.TimeoutExpired: os.killpg(proc.pid,signal.SIGKILL); proc.wait()
  os.close(master)
 print(json.dumps({"native_ready":ready,"native_completed_preference":completed,"account_route":"apiKeyHelper; native API Usage Billing banner","actions":actions,"screen":raw.decode(errors="replace"),"model_prompt_sent":False,"fabricated_onboarding_marker":False,"exit":proc.returncode}))
 return 0 if ready else 1

if __name__=="__main__": raise SystemExit(main())
