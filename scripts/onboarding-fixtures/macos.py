"""Manual Tart lifecycle and macOS assertions for the shared onboarding runner."""
import collections
import ipaddress
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import time
import uuid

LABEL = 'com.compoundingtech.st3'
APP = '/Users/ada/Applications/SmallTalk.app'

def daemon_running(status):
    return bool(status and status.get('manager')=='launchd-user' and any(
        s.get('name')==LABEL and s.get('installed') and s.get('running') and s.get('state')=='running'
        for s in status.get('services',[])))

class TartMachine:
    def __init__(self,args,out):
        self.args,self.out=args,out
        self.name='st-onboarding-'+uuid.uuid4().hex[:12]
        self.created=False
        self.process=None
        self.log=None
        self.address=None
        self.host_commands=[]

    def host(self,*args,timeout=60):
        start=time.monotonic()
        try:
            r=subprocess.run(['tart',*args],capture_output=True,text=True,timeout=timeout,
                env={**os.environ,'TART_NO_AUTO_PRUNE':'1'})
            result={'argv':['tart',*args],'exit':r.returncode,'stdout':r.stdout,'stderr':r.stderr}
        except subprocess.TimeoutExpired as e:
            result={'argv':['tart',*args],'exit':124,'stdout':(e.stdout or b'').decode(errors='replace'),'stderr':(e.stderr or b'').decode(errors='replace')}
        result['seconds']=round(time.monotonic()-start,3)
        self.host_commands.append(result)
        return result

    def boot(self):
        self.log=open(self.out/'tart-run.log','ab')
        self.process=subprocess.Popen(['tart','run',self.name],stdin=subprocess.DEVNULL,
            stdout=self.log,stderr=subprocess.STDOUT,start_new_session=True,
            env={**os.environ,'TART_NO_AUTO_PRUNE':'1'})
        self.host_commands.append({'argv':['tart','run',self.name],'pid':self.process.pid,'log':'tart-run.log'})
        deadline=time.monotonic()+180
        while time.monotonic()<deadline:
            if self.process.poll() is not None: raise RuntimeError('tart run exited; see tart-run.log')
            r=self.host('ip',self.name,timeout=10)
            if not r['exit']:
                try: self.address=str(ipaddress.ip_address(r['stdout'].strip()))
                except ValueError: raise RuntimeError('Tart returned an invalid guest IP')
                r=self.run('test "$(id -un)" = ada && test "$(stat -f %Su /dev/console)" = ada && launchctl print "gui/$(id -u)" >/dev/null',timeout=10)
                if r['exit']==0: return
            time.sleep(2)
        raise RuntimeError('guest SSH did not become ready; prepare ada/key/Remote Login in the clean base')

    def start(self):
        r=self.host('clone',self.args.tart_image,self.name,timeout=300)
        if r['exit']: raise RuntimeError('Tart clone failed: '+r['stderr'])
        self.created=True
        r=self.host('set',self.name,'--cpu','2','--memory','4096')
        if r['exit']: raise RuntimeError('Tart configuration failed: '+r['stderr'])
        self.boot()

    def ssh(self):
        if self.address is None: raise RuntimeError('guest has no address')
        return ['ssh','-T','-F','/dev/null','-i',str(self.args.ssh_key.resolve()),'-o','BatchMode=yes',
            '-o','IdentitiesOnly=yes','-o','PasswordAuthentication=no','-o','KbdInteractiveAuthentication=no',
            '-o','ConnectTimeout=5','-o','StrictHostKeyChecking=accept-new',
            '-o','UserKnownHostsFile='+str((self.out/'known_hosts').resolve()),'ada@'+self.address]

    def run(self,script,timeout=75):
        start=time.monotonic()
        # SSH takes one remote command string; all dynamic values must be shell-quoted.
        remote=shlex.join(['/bin/bash','-l','-s'])
        try:
            r=subprocess.run(self.ssh()+[remote],input=script.encode(),capture_output=True,timeout=timeout)
            result={'script':script,'exit':r.returncode,'stdout':r.stdout.decode(errors='replace'),'stderr':r.stderr.decode(errors='replace')}
        except subprocess.TimeoutExpired as e:
            result={'script':script,'exit':124,'stdout':(e.stdout or b'').decode(errors='replace'),'stderr':(e.stderr or b'').decode(errors='replace')}
        result['seconds']=round(time.monotonic()-start,3)
        return result

    def send(self,data,path):
        remote=shlex.join(['/bin/bash','-c','mkdir -p ~/smalltalk-install; cat > '+shlex.quote(path)])
        r=subprocess.run(self.ssh()+[remote],input=data,capture_output=True,timeout=60)
        if r.returncode: raise RuntimeError('copy failed: '+r.stderr.decode(errors='replace'))

    def stop(self):
        if self.process is not None:
            if self.process.poll() is None:
                r=self.host('stop',self.name,'--timeout','30')
                if r['exit']: raise RuntimeError('Tart stop failed: '+r['stderr'])
            self.process.wait(timeout=40)
            self.process=None
        if self.log: self.log.close(); self.log=None

    def reboot(self):
        self.stop()
        self.address=None
        self.boot()

    def close(self):
        if not self.created: return
        if self.args.keep:
            if self.log: self.log.close(); self.log=None
            return
        self.stop()
        r=self.host('delete',self.name)
        if r['exit']: raise RuntimeError('Tart cleanup failed: '+r['stderr'])
        self.created=False

class MacRun:
    def __init__(self,machine,args,scenario,archive,metadata,probe):
        self.m,self.args,self.scenario,self.archive,self.metadata,self.probe=machine,args,scenario,archive,metadata,probe
        self.commands=[]; self.checks=[]
        self.st='/Users/ada/.local/bin/st'
        self.signing={}
        if args.macos_signing_identity: self.signing['ST_MACOS_SIGNING_IDENTITY']=args.macos_signing_identity
        if args.macos_signing_team: self.signing['ST_MACOS_SIGNING_TEAM']=args.macos_signing_team

    def save(self):
        result={'platform':'macOS','scenario':self.scenario,'backend':'tart','machine':self.m.name,
            'base':self.args.tart_image,'cpus':2,'memory_mib':4096,'host_mounts':False,
            'archive':self.metadata,'checks':self.checks,'commands':self.commands,'host_commands':self.m.host_commands,
            'retained':self.args.keep,'reboot_skipped':self.args.no_reboot,'limits':['No automatic TCC approval; guidance is not proof of file/developer permission grants.','GUI ada auto-login is a base prerequisite; reboot checks service after GUI login, not before login.','No provider, model, mission-content or signing-across-changed-build proof.']}
        (self.m.out/'result.json').write_text(json.dumps(result,indent=2)+'\n')
        lines=['# macOS onboarding / '+self.scenario,'','VM: `'+self.m.name+'`. Archive SHA256: `'+self.metadata['sha256']+'`.','']
        lines += ['- **'+c['status']+'** `'+c['id']+'`: '+c['detail'] for c in self.checks]
        for c in self.commands:
            lines += ['', '## '+c['title'],'','```sh',c['script'],'```','',f"Exit {c['exit']}, {c['seconds']} seconds.",'','```',c['stdout']+c['stderr'],'```']
        lines += ['', '## Host lifecycle','', '```json',json.dumps(self.m.host_commands,indent=2),'```','', '## Limits','']+result['limits']
        (self.m.out/'report.md').write_text('\n'.join(lines)+'\n')

    def check(self,key,ok,detail):
        self.checks.append({'id':key,'status':'PASS' if ok else 'FAIL','detail':detail})
        print('macOS/'+self.scenario+': '+self.checks[-1]['status']+' '+key+': '+detail,flush=True)
        self.save(); return ok

    def execute(self,title,script,timeout=75):
        r=self.m.run(script,timeout=timeout); r['title']=title; self.commands.append(r); self.save(); return r

    def cli(self,title,*argv,timeout=75):
        prefix=['env',*[k+'='+v for k,v in self.signing.items()]] if self.signing else []
        return self.execute(title,shlex.join([*prefix,self.st,*argv]),timeout)

    def text(self,r): return r['stdout']+r['stderr']

    def identity(self,title):
        code='''import json,os,plistlib,subprocess
from pathlib import Path
app=Path.home()/"Applications/SmallTalk.app"
st=Path.home()/".local/bin/st"
info=plistlib.loads((app/"Contents/Info.plist").read_bytes())
r=subprocess.run(["/usr/bin/codesign","-d","-r-","--verbose=2",str(app)],capture_output=True,text=True,check=True)
text=r.stdout+r.stderr
requirement=next((x.lstrip("# ") for x in text.splitlines() if x.lstrip("# ").startswith("designated => ")),None)
print(json.dumps({"app":str(app),"cli_realpath":str(st.resolve()),"executable":info.get("CFBundleExecutable"),"identifier":info.get("CFBundleIdentifier"),"requirement":requirement,"team":next((x.split("=",1)[1] for x in text.splitlines() if x.startswith("TeamIdentifier=")),None),"stui_absent":not (app/"Contents/MacOS/stui").exists() and not os.path.lexists(Path.home()/".local/bin/stui")}))'''
        r=self.execute(title,shlex.join(['python3','-c',code]))
        try: return json.loads(r['stdout']) if not r['exit'] else None
        except ValueError: return None

    def service(self,title):
        deadline=time.monotonic()+20
        while True:
            r=self.cli(title,'service','status','--json')
            try:
                value=json.loads(r['stdout']) if not r['exit'] else None
                status=value.get('value',value) if isinstance(value,dict) else None
            except ValueError: status=None
            if daemon_running(status) or time.monotonic()>=deadline: return status
            time.sleep(1)

    def setup(self):
        r=self.execute('Fresh Mac and GUI session','set -e\nuname -sm\nsw_vers\nid\ntest "$HOME" = /Users/ada\ntest "$(id -un)" = ada\ntest "$(stat -f %Su /dev/console)" = ada\nlaunchctl print "gui/$(id -u)" >/dev/null\ncommand -v python3\ntest ! -e ~/.config/st3 && test ! -e ~/.local/state/st3 && test ! -e ~/Applications/SmallTalk.app && test ! -e ~/Library/LaunchAgents/com.compoundingtech.st3.plist')
        if not self.check('fresh-mac',r['exit']==0 and 'Darwin arm64' in r['stdout'],'clean ada home on Apple Silicon macOS with an actual GUI launchd domain'): return
        r=self.execute('No providers or developer tools','for t in gh cargo rustc mold sccache nix claude codex opencode pi omp; do command -v "$t" || true; done')
        if not self.check('clean-tool-path',r['exit']==0 and not r['stdout'].strip(),'no gh, developer toolchain or provider initially installed'): return
        self.m.send(self.archive.read_bytes(),'/Users/ada/smalltalk-install/candidate.tar.gz')
        r=self.execute('Extract verified Mac archive','set -e\ncd ~/smalltalk-install\nshasum -a 256 candidate.tar.gz\ntar -xzf candidate.tar.gz\ncat '+shlex.quote(self.metadata['package']+'/BUILD.json'))
        if not self.check('archive-extracted',r['exit']==0 and r['stdout'].split()[0]==self.metadata['sha256'],'copied bytes match recorded archive and BUILD.json captured'): return
        installer='/Users/ada/smalltalk-install/'+self.metadata['package']+'/install.sh'
        env=['env',*[k+'='+v for k,v in self.signing.items()]]
        r=self.execute('Extracted Mac installer',shlex.join([*env,installer,'--bin-dir','/Users/ada/.local/bin']))
        if not self.check('guide-install',r['exit']==0,'archive installer completes without sudo or password entry'): return
        r=self.execute('App signature verification','/usr/bin/codesign --verify --strict --deep '+shlex.quote(APP))
        self.check('app-signature',r['exit']==0,'installed bundle passes strict deep codesign verification')
        identity=self.identity('Installed bundle identity')
        self.check('app-identity',bool(identity and identity['cli_realpath']==APP+'/Contents/MacOS/st3' and identity['executable']=='st3' and identity['identifier']=='com.compoundingtech.smalltalk' and identity['requirement']),'st resolves to the fixed app executable with a designated requirement')
        self.check('stui-absent',bool(identity and identity['stui_absent']),'retired executable absent from app and CLI directory')
        if self.args.macos_signing_team:
            self.check('signing-team',bool(identity and identity['team']==self.args.macos_signing_team),'installed TeamIdentifier matches the explicitly configured team')
        signing=shlex.join(env)+' ' if self.signing else ''
        if self.scenario=='first-run':
            r=self.execute('Plain st first-run PTY',shlex.join([*env,'python3','-c',self.probe,json.dumps({'argv':[self.st],'timeout':55})]),timeout=65)
            try: p=json.loads(r['stdout'])
            except ValueError: p={}
            self.check('first-run-questions',all(x in p.get('answers',[]) for x in ('Your name',"This machine's name",'Keep st running in the background')),'three initial questions answered in a real guest PTY')
            self.check('tui-frame',p.get('frame') and p.get('home_frame') and p.get('quit_sent') and p.get('restored') and p.get('exit')==0,'visible Home/Now, Ctrl+Q and terminal restoration')
        else:
            self.execute('Config merge sentinel',"mkdir -p ~/.config/st3; printf '[checkpoint]\\nenabled = false\\n' > ~/.config/st3/config.toml")
            r=self.execute('Setup flags',signing+shlex.join([self.st,'setup','--person','ada','--node','studio','--yes','--service','true']))
            if not self.check('setup-success',r['exit']==0,'real candidate setup completed'): return
        code='import os,tomllib; from pathlib import Path; c=tomllib.loads((Path.home()/".config/st3/config.toml").read_text()); assert c["person"]=="person/ada"; assert c["node"]=="studio"; '+('assert c["checkpoint"]["enabled"] is False' if self.scenario!='first-run' else 'assert True')
        r=self.execute('Persisted identity and config',shlex.join(['python3','-c',code]))
        self.check('configured-person-node',r['exit']==0,'ada/studio persisted and existing config preserved')
        r=self.execute('Fresh login PATH','command -v st; test "$(command -v st)" = "$HOME/.local/bin/st"')
        self.check('login-path',r['exit']==0,'st resolves in a fresh login shell')
        status=self.service('Launchd service status')
        self.check('launchd-active',daemon_running(status),'actual launchd user daemon running')
        code='import json,plistlib; from pathlib import Path; p=plistlib.loads((Path.home()/"Library/LaunchAgents/com.compoundingtech.st3.plist").read_bytes()); print(json.dumps(p)); assert p["Label"]=="com.compoundingtech.st3"; assert p["ProgramArguments"][0]==str(Path.home()/"Applications/SmallTalk.app/Contents/MacOS/st3")'
        r=self.execute('LaunchAgent app executable',shlex.join(['python3','-c',code]))
        self.check('launchd-app-path',r['exit']==0,'LaunchAgent executes the installed fixed app binary')
        r=self.cli('Ordinary Mac doctor','doctor')
        self.check('doctor-healthy',r['exit']==0,'ordinary doctor succeeds without gh or a toolchain')
        r=self.cli('Mac service permissions guidance','service','permissions')
        self.check('permissions-guidance',r['exit']==0 and all(x in self.text(r) for x in ('Full Disk Access','Developer Tools','st service restart',APP+'/Contents/MacOS/st3')),'guidance names actual app executable and manual approvals; no approval is inferred')
        r=self.cli('No harness setup explanation','setup','--person','ada','--node','studio','--yes','--service','true')
        self.check('no-harness-explained',r['exit']==0 and 'No supported harness is installed' in self.text(r) and 'No onboarding seat was created' in self.text(r),'missing providers are explained without creating onboarding')
        for kind,subject in [('agents','agent/st/expert'),('missions','mission/st/onboarding')]:
            r=self.cli('No built-in '+kind,kind,'ls','--all','--json')
            try: page=json.loads(r['stdout']); page=page.get('value',page); absent=not any(x['id']==subject for x in page['items'])
            except (ValueError,KeyError,TypeError): absent=False
            self.check('no-harness-no-'+kind,r['exit']==0 and absent,'no built-in '+subject+' without a harness')
        if self.scenario!='first-run':
            r=self.execute('Plain installed st PTY',shlex.join(['python3','-c',self.probe,json.dumps({'argv':[self.st],'timeout':55})]),timeout=65)
            try: p=json.loads(r['stdout'])
            except ValueError: p={}
            self.check('tui-frame',p.get('frame') and p.get('home_frame') and p.get('quit_sent') and p.get('restored') and p.get('exit')==0,'visible Home/Now, Ctrl+Q and terminal restoration')
        r=self.execute('Repeat extracted Mac installer',shlex.join([*env,installer,'--bin-dir','/Users/ada/.local/bin']))
        after=self.identity('Bundle identity after repeated install')
        self.check('install-idempotent',r['exit']==0 and identity is not None and identity==after,'same-payload reinstall preserves app identity and retired executable absence')
        status=self.service('Launchd status after repeated install')
        self.check('service-after-reinstall',daemon_running(status),'daemon remains running after installer rerun')
        if self.args.strict_release:
            r=self.cli('Release version and stderr','--version')
            self.check('release-version',r['exit']==0 and not r['stderr'] and 'running from local source' not in r['stdout'],'release-looking version and clean stderr')
        r=self.execute('Guest PTY version','~/.local/bin/pty --version')
        self.check('pty-executable',r['exit']==0,'installed PTY executes on native macOS')
        if not self.args.no_reboot:
            before=self.execute('Boot UUID before power cycle','sysctl -n kern.bootsessionuuid')
            self.m.reboot()
            after=self.execute('Boot UUID and GUI user after power cycle','sysctl -n kern.bootsessionuuid; stat -f %Su /dev/console')
            self.check('guest-reboot',before['exit']==0 and after['exit']==0 and bool(before['stdout'].strip()) and before['stdout'].strip()!=after['stdout'].splitlines()[0] and after['stdout'].splitlines()[-1]=='ada','Tart stop/run changes guest boot UUID; ada GUI login restored')
            status=self.service('Launchd service after GUI login on new boot')
            self.check('service-after-reboot',daemon_running(status),'daemon returns after GUI auto-login on actual new guest boot')


def run_tart(args,archive,metadata,probe):
    failed=False
    for scenario in args.scenario or [args.mode or 'setup']:
        out=args.out/('macos-'+scenario); out.mkdir()
        machine=TartMachine(args,out); run=MacRun(machine,args,scenario,archive,metadata,probe)
        try:
            machine.start(); run.setup()
        except Exception as error: run.check('runner-error',False,str(error))
        finally:
            try: machine.close()
            except Exception as error: run.check('cleanup',False,str(error))
            run.save()
        counts=collections.Counter(c['status'] for c in run.checks)
        print('macOS/'+scenario+': '+json.dumps(counts),flush=True)
        failed |= any(c['status']=='FAIL' for c in run.checks)
    return int(failed)
