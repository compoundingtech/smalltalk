#!/usr/bin/env python3
"""UNRUN, hosted-only controller. Job-local deadline is not a queued-job canceller."""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import selectors
import signal
import subprocess
import sys
import tempfile
import time
from urllib.request import Request, urlopen

PROBE_SHA='9c6536f94a56aee34444cc3344a382ea1adc013950ad13bc4917dae30e107574'
DISK=268435456
MEMORY=536870912
HIGH=268435456
OUTPUT=262144
HERE=Path(__file__).resolve().parent
class Refused(RuntimeError):pass
class CaptureStopped(Refused):
    def __init__(self,reason,data):
        super().__init__(reason);self.data=bytes(data)
def require(value,reason):
    if not value:raise Refused(reason)
def sha(path):return hashlib.sha256(Path(path).read_bytes()).hexdigest()
def epoch(text):return datetime.datetime.fromisoformat(text.replace('Z','+00:00')).timestamp()
def api(path):
    require(os.environ.get('GH_TOKEN'),'missing read-only metadata credential')
    req=Request('https://api.github.com/repos/compoundingtech/smalltalk/'+path,
                headers={'Authorization':'Bearer '+os.environ['GH_TOKEN'],'Accept':'application/vnd.github+json'})
    with urlopen(req,timeout=2) as response:data=response.read(2097153)
    require(len(data)<=2097152,'metadata byte cap exceeded');return json.loads(data)
def binaries():
    import sqlite3
    import _sqlite3
    libs={str(Path(line.split()[-1]).resolve()) for line in Path('/proc/self/maps').read_text().splitlines()
          if line.split()[-1].startswith('/') and 'libsqlite3.so' in line.split()[-1]}
    require(len(libs)==1,'missing/ambiguous mapped SQLite library')
    files={'python':str(Path(sys.executable).resolve()),'_sqlite3':str(Path(_sqlite3.__file__).resolve()),'sqlite':libs.pop()}
    return {'files':{k:{'path':p,'sha256':sha(p)} for k,p in files.items()},'python_version':sys.version,'sqlite_version':sqlite3.sqlite_version}
def setup(argv,deadline):
    require(time.monotonic()<deadline,'setup deadline expired')
    subprocess.run(argv,check=True,stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,timeout=min(2,deadline-time.monotonic()))
def capture(process,deadline,limit):
    data=bytearray();sel=selectors.DefaultSelector();sel.register(process.stdout,selectors.EVENT_READ)
    try:
        while sel.get_map():
            require(time.monotonic()<deadline,'owned process/output deadline reached')
            for key,_ in sel.select(timeout=min(.1,max(0,deadline-time.monotonic()))):
                chunk=os.read(key.fileobj.fileno(),65536)
                if not chunk:sel.unregister(key.fileobj);continue
                if len(data)+len(chunk)>limit:
                    data.extend(chunk[:limit-len(data)]);raise Refused('output byte cap exceeded; retained prefix')
                data.extend(chunk)
        process.wait(timeout=max(.01,min(1,deadline-time.monotonic())))
        return bytes(data)
    except BaseException as error:raise CaptureStopped(str(error),data) from error
    finally:sel.close()
def stop(process,deadline):
    if process is not None and process.poll() is None:
        os.killpg(process.pid,signal.SIGKILL)
        process.wait(timeout=max(.01,min(1,deadline-time.monotonic())))
    if process is not None and process.stdout:process.stdout.close()
def inner(m):
    require(os.geteuid()==0 and os.getpid()==1,'private privileged PID/mount provider missing')
    require(os.readlink('/proc/self/ns/mnt')!=m['parent_mount_ns'],'mount namespace was not isolated')
    deadline=m['deadline'];quota=Path(m['quota']);group=None;owned_group=False;parent=None;mounted=False;aliases=[];child=None
    result={'status':'refused','run_id':m['run_id'],'source_head':m['head'],'deadline_epoch':m['deadline_epoch'],'deadline_monotonic':deadline,'cleanup':{}}
    try:
        setup(['/usr/bin/mount','--make-rprivate','/'],deadline)
        cg=Path('/sys/fs/cgroup');require((cg/'cgroup.controllers').is_file(),'cgroup-v2 provider missing')
        location=[s[3:] for s in Path('/proc/self/cgroup').read_text().splitlines() if s.startswith('0::')]
        require(len(location)==1,'current delegated cgroup identity missing')
        parent=cg/location[0].lstrip('/');require(parent.is_dir(),'delegated cgroup not visible')
        group=parent/m['scope'];group.mkdir();owned_group=True
        for name,value in [('memory.max',MEMORY),('memory.high',HIGH),('memory.swap.max',0),('pids.max',16)]:
            (group/name).write_text(str(value));require((group/name).read_text().strip()==str(value),'resource limit did not apply')
        (group/'cgroup.procs').write_text('0')
        result['resource_limits']={k:(group/k).read_text().strip() for k in ('memory.max','memory.high','memory.swap.max','pids.max')}
        setup(['/usr/bin/mount','-t','tmpfs','-o',f'size={DISK},mode=0700,nosuid,nodev,noexec','receive-f1',str(quota)],deadline);mounted=True
        entries=[line for line in Path('/proc/self/mountinfo').read_text().splitlines() if line.split(' - ')[0].split()[4]==str(quota)]
        require(len(entries)==1 and entries[0].split(' - ')[1].split()[0]=='tmpfs','actual mount is not tmpfs')
        fs=os.statvfs(quota);require(fs.f_blocks*fs.f_frsize==DISK,'actual hard tmpfs capacity is not256MiB')
        result['quota']={'mountinfo':entries[0],'capacity_bytes':fs.f_blocks*fs.f_frsize,'free_bytes':fs.f_bavail*fs.f_frsize,'sampled_sizes_are_not_quota':True}
        # All Unix SQLite temp fallbacks/cwd point at the SAME quota, not independent mounts.
        for target in ('/tmp','/var/tmp','/usr/tmp','/dev/shm'):
            if Path(target).is_dir():setup(['/usr/bin/mount','--bind',str(quota),target],deadline);aliases.append(target)
        facts=binaries();require(facts==m['binary_facts'],'binary facts changed during setup')
        require({name:sha(HERE/name) for name in m['source_files']}==m['source_files'],'source binding changed during setup')
        require(sha(HERE/'probe.py')==PROBE_SHA,'exact reviewed growth-source hash mismatch')
        for file in [Path(x['path']) for x in facts['files'].values()]+[HERE/'probe.py',HERE/'ratios.py',HERE/'controller.py']:
            setup(['/usr/bin/mount','--bind',str(file),str(file)],deadline)
            setup(['/usr/bin/mount','-o','remount,bind,ro',str(file)],deadline)
        require(deadline-time.monotonic()>12,'no remaining execution plus cleanup budget')
        result['binary_facts']=facts;result['probe_sha256']=PROBE_SHA
        env={'PATH':'/usr/bin:/bin','LANG':'C.UTF-8','TMPDIR':'/tmp','SQLITE_TMPDIR':'/tmp','PYTHONDONTWRITEBYTECODE':'1','PYTHONNOUSERSITE':'1'}
        child=subprocess.Popen([facts['files']['python']['path'],'-I',str(HERE/'probe.py')],cwd='/tmp',env=env,
                               stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,start_new_session=True)
        try:raw=capture(child,deadline-10,OUTPUT)
        except CaptureStopped as error:
            result['raw']=error.data.decode('utf-8',errors='replace');result['raw_sha256']=hashlib.sha256(error.data).hexdigest()
            raise
        result['raw']=raw.decode('utf-8',errors='replace');result['raw_sha256']=hashlib.sha256(raw).hexdigest();result['returncode']=child.returncode
        stop(child,deadline-8)
        cases=[]
        for line in result['raw'].splitlines():
            try:item=json.loads(line)
            except json.JSONDecodeError:continue
            if 'completed_case' in item:cases.append(item['completed_case'])
        sys.path.insert(0,str(HERE));import ratios
        result['ratios']=ratios.report(cases)
        require(binaries()==facts,'binary facts changed during execution')
        require({name:sha(HERE/name) for name in m['source_files']}==m['source_files'],'source binding changed during execution')
        require(child.returncode==0 and len(cases)==6,'genuine error/cap/incomplete selection; no retry')
        result['status']='mechanism-complete'
    except BaseException as error:result['reason']=str(error)
    finally:
        try:stop(child,deadline-4)
        except BaseException as error:result['cleanup']['process_error']=str(error)
        if owned_group:
            try:
                (parent/'cgroup.procs').write_text('0')
                if (group/'cgroup.procs').read_text().strip():(group/'cgroup.kill').write_text('1')
                require(not (group/'cgroup.procs').read_text().strip(),'owned delegated processes remain')
                group.rmdir();result['cleanup']['cgroup']='empty and removed'
            except BaseException as error:result['cleanup']['cgroup_error']=str(error)
        if mounted:
            try:
                for target in reversed(aliases):setup(['/usr/bin/umount',target],deadline-2)
                result['cleanup']['remaining_owned_entries']=sum(1 for _ in quota.rglob('*'))
                setup(['/usr/bin/umount',str(quota)],deadline-1);result['cleanup']['tmpfs']='unmounted'
            except BaseException as error:result['cleanup']['mount_error']=str(error)
        result['terminal_epoch']=time.time()
        if time.monotonic()>deadline or any(k.endswith('_error') for k in result['cleanup']):result['status']='deadline-or-cleanup-failed'
        print(json.dumps(result,sort_keys=True),flush=True)
    return 0 if result['status']=='mechanism-complete' else 1
def outer():
    require(os.environ.get('GITHUB_ACTIONS')=='true' and os.environ.get('RUNNER_OS')=='Linux','not hosted Linux; local/production invocation refused')
    require(os.environ.get('GITHUB_EVENT_NAME')=='pull_request' and os.environ.get('GITHUB_RUN_ATTEMPT')=='1','only normal first-attempt PR admission')
    resource.setrlimit(resource.RLIMIT_AS,(MEMORY,MEMORY))
    event=json.loads(Path(os.environ['GITHUB_EVENT_PATH']).read_text());pr=event['pull_request'];head=pr['head']['sha'];run_id=os.environ['GITHUB_RUN_ID']
    require(event['action']=='ready_for_review' and pr['number']==2864 and not pr['draft'],'not the bound Ready event')
    require(pr['head']['repo']['full_name']=='compoundingtech/smalltalk' and re.fullmatch('[a-f0-9]{40}',head) and run_id.isdigit(),'source/run identity missing')
    run=api('actions/runs/'+run_id);current=api('pulls/2864')
    jobs=api('actions/runs/'+run_id+'/jobs?per_page=10')
    require(jobs.get('total_count')==1 and len(jobs.get('jobs',[]))==1,'not the one-job unit')
    job=jobs['jobs'][0]
    require(job['name']=='receive-replay-f1' and job['status']=='in_progress' and job.get('runner_name'),'actual runner identity missing')
    require(any(str(label).startswith('namespace-profile-linux-x86-64;job.priority=2;') for label in job.get('labels',[])),'ordinary ephemeral Namespace PR provider not attested')
    require(run['event']=='pull_request' and run['head_sha']==head and current['head']['sha']==head and not current['draft'],'source superseded')
    created=min(epoch(run['created_at']),epoch(pr['updated_at']));deadline_epoch=created+90
    deadline=time.monotonic()+(deadline_epoch-time.time())
    require(created<=time.time()+1 and deadline-time.monotonic()>15,'queued/setup deadline expired; no invocation')
    require(subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip()==head,'checkout is not exact PR head')
    require(sha(HERE/'probe.py')==PROBE_SHA,'probe source differs from reviewed9c6536f9')
    require(not any(HERE.is_relative_to(Path(p)) for p in ('/tmp','/var/tmp','/usr/tmp','/dev/shm')),'checkout would be hidden by private temp mounts')
    require(not any(x in os.uname().nodename.lower() for x in ('hetz','ci1','ci2','silber','bluey')),'production/shared host refused')
    temp=Path(os.environ['RUNNER_TEMP']).resolve()
    require(not any(temp.is_relative_to(Path(p)) for p in ('/tmp','/var/tmp','/usr/tmp','/dev/shm')),'owned quota path would be hidden by private mounts')
    source_files={name:sha(HERE/name) for name in ('probe.py','ratios.py','controller.py')}
    facts=binaries()
    with tempfile.TemporaryDirectory(prefix='receive-f1-controller-',dir=os.environ['RUNNER_TEMP']) as owned:
        root=Path(owned);quota=root/'quota';quota.mkdir()
        m={'deadline':deadline,'deadline_epoch':deadline_epoch,'quota':str(quota),'binary_facts':facts,'head':head,'run_id':run_id,
           'source_files':source_files,'provider_job':job,'parent_mount_ns':os.readlink('/proc/self/ns/mnt'),'scope':'receive-f1-'+run_id+'-'+head[:12]}
        path=root/'manifest.json';path.write_text(json.dumps(m));path.chmod(0o600)
        process=subprocess.Popen(['/usr/bin/sudo','-n','/usr/bin/unshare','--mount','--pid','--fork','--kill-child=KILL','--mount-proc','--',
                                  facts['files']['python']['path'],'-I',str(Path(__file__).resolve()),'--inner',str(path)],
                                 stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,start_new_session=True)
        raw=b'';failure=None
        try:raw=capture(process,deadline-2,8*OUTPUT)
        except CaptureStopped as error:raw=error.data;failure=str(error)
        finally:
            try:stop(process,deadline)
            except BaseException as error:failure=str(error)
            artifact=temp/'receive-replay-f1-result.json';artifact.write_bytes(raw)
            if failure:
                (temp/'receive-replay-f1-controller-failure.json').write_text(json.dumps({'status':'retained-stop','reason':failure,'raw_sha256':hashlib.sha256(raw).hexdigest(),'cleanup':'namespace exited or uncertain; no cleanup success inferred','terminal_epoch':time.time()}))
        require(failure is None,failure or 'capture failed')
        require(time.monotonic()<=deadline,'physical namespace cleanup exceeded cutoff')
        require(subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip()==head,'source-after changed')
        require({name:sha(HERE/name) for name in source_files}==source_files,'source file bytes changed')
        print(json.dumps({'artifact':str(artifact),'raw_sha256':hashlib.sha256(raw).hexdigest(),'head':head,'run_id':run_id,
                          'source_files':source_files,'provider_job':job,'absolute_deadline_epoch':deadline_epoch,'deadline_monotonic':deadline,'terminal_epoch':time.time(),'returncode':process.returncode,'binary_facts':facts,
                          'queue_limit':'expired queues refuse invocation; job-local code cannot cancel unscheduled work'}))
        return process.returncode
if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--inner',type=Path);args=parser.parse_args()
    try:code=inner(json.loads(args.inner.read_text())) if args.inner else outer()
    except BaseException as error:
        refusal={'status':'refused','reason':str(error),'terminal_epoch':time.time(),'invocation_or_cleanup_success':False}
        if not args.inner and os.environ.get('GITHUB_ACTIONS')=='true' and os.environ.get('RUNNER_TEMP'):
            (Path(os.environ['RUNNER_TEMP'])/'receive-replay-f1-controller-failure.json').write_text(json.dumps(refusal))
        print(json.dumps(refusal),flush=True);code=1
    raise SystemExit(code)
