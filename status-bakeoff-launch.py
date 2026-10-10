"""Launch only owned scratch daemons/proxies/TUIs, with kernel-notified socket readiness."""
import ctypes,fcntl,json,os,select,struct,subprocess,sys,termios,time,tomllib
from pathlib import Path
CASE,HOST,MODE=sys.argv[1:4]
if CASE not in ['ws-local','lp-local','ws-cross','lp-cross','ws-race','lp-race','ws-tuned-local','ws-tuned-cross','ws-tuned-race'] or HOST not in ['dev3','dev5'] or MODE not in ['server','ui','all']:raise SystemExit('invalid isolated case')
ROOT=Path(f'/tmp/status-bakeoff-{CASE}-{HOST}');ROOT.mkdir(parents=True,exist_ok=True)
for d in ['state','pty','ui-state','ui-cache','ui-config']:(ROOT/d).mkdir(exist_ok=True)
ART=Path('/tmp/status-bakeoff-artifacts');HERE=Path(__file__).parent
libc=ctypes.CDLL(None,use_errno=True)
def launch(part,command,extra_env=None):
    session=f'status-bakeoff.{CASE}.{HOST}.{part}'
    args=['pty','run','-d','--id',session,'--tag','keep=true','--unset-env','ST_AGENT','--unset-env','ST_ACTOR','--unset-env','FRACTAL_ST_PERSON']
    for key,value in (extra_env or {}).items():args+=['--env',key+'='+value]
    p=subprocess.run(args+['--']+list(map(str,command)),text=True,capture_output=True)
    if p.returncode:raise RuntimeError(p.stdout+p.stderr)
    print(p.stdout.strip(),flush=True)
    info=json.loads(subprocess.check_output(['pty','stats','--json',session],text=True));(ROOT/(part+'-receipt.json')).write_text(json.dumps(info,indent=2)+'\n')
    if not info['process']['alive']:raise RuntimeError('scratch '+part+' exited: '+subprocess.check_output(['pty','peek','--plain',session],text=True))
    return {'session':session,'pid':info['process']['pid']}
def await_socket(path,deadline=90):
    if path.exists():return
    fd=libc.inotify_init1(os.O_NONBLOCK|os.O_CLOEXEC)
    if fd<0:raise OSError(ctypes.get_errno(),'inotify_init1')
    try:
        if libc.inotify_add_watch(fd,str(path.parent).encode(),0x100|0x80)<0:raise OSError(ctypes.get_errno(),'inotify_add_watch')
        end=time.monotonic()+deadline
        while not path.exists():
            remaining=end-time.monotonic()
            if remaining<=0 or not select.select([fd],[],[],remaining)[0]:raise TimeoutError('scratch socket readiness '+str(path))
            os.read(fd,65536)
    finally:os.close(fd)
result={'case':CASE,'host':HOST,'root':str(ROOT),'wall_ms':time.time_ns()/1e6}
if MODE in ['server','all']:
    config={'node':'status-bakeoff-'+HOST,'state_dir':str(ROOT/'state'),'socket':str(ROOT/'st.sock'),'client_gateway_socket':str(ROOT/'gateway.sock'),'pty_root':str(ROOT/'pty')}
    if 'cross' in CASE:
        base=tomllib.loads(Path(f'/tmp/status-bakeoff-{HOST}/config.toml').read_text())
        config.update({key:base[key] for key in ['fleet_id','shared_secret_file','peer_listen']})
        text=''.join(key+' = '+json.dumps(value)+'\n' for key,value in config.items())
        text+='[[peers]]\n'+''.join(key+' = '+json.dumps(value)+'\n' for key,value in base['peers'][0].items())
    else:text=''.join(key+' = '+json.dumps(value)+'\n' for key,value in config.items())
    (ROOT/'config.toml').write_text(text)
    if HOST=='dev3':
        fixture_env=dict(os.environ,PTY_ROOT=str(ROOT/'pty'),PTY_SESSION_DIR=str(ROOT/'pty'))
        fixture_env.pop('ST_AGENT',None);fixture_env.pop('ST_ACTOR',None)
        subprocess.run(['pty','run','-d','--id','status-bakeoff-fixture','--tag','keep=true','--','sleep','7200'],env=fixture_env,check=True)
        fixture=json.loads(subprocess.check_output(['pty','stats','--json','status-bakeoff-fixture'],env=fixture_env,text=True))
        (ROOT/'fixture-receipt.json').write_text(json.dumps(fixture,indent=2)+'\n')
        result['fixture']={'session':'status-bakeoff-fixture','pid':fixture['process']['pid'],'registry':str(ROOT/'pty')}
    server_env={'ST_BAKEOFF_AGENTS_WS_IMMEDIATE':'1'} if 'tuned' in CASE else None
    result['daemon']=launch('daemon',[ART/'st3','up','--config',ROOT/'config.toml'],server_env);await_socket(ROOT/'st.sock')
    if 'cross' in CASE:result['replication']=launch('replication',[ART/'st3','replication-worker','--config',ROOT/'config.toml'])
    observer_env={'BAKEOFF_ACK_BIND':'100.124.235.29:33174','BAKEOFF_ACK_SECRET_FILE':'/tmp/status-bakeoff-dev5/fleet-secret'} if 'cross' in CASE and HOST=='dev5' else None
    result['observer']=launch('observer',['python3',HERE/'status-bakeoff-publications.py',ROOT/'st.sock',ROOT/'publications.jsonl'],observer_env)
if MODE in ['ui','all']:
    await_socket(ROOT/'st.sock')
    result['proxy']=launch('proxy',['python3',HERE/'status-bakeoff-wire.py',ROOT/'st.sock',ROOT/'proxy.sock',ROOT/'wire.jsonl']);await_socket(ROOT/'proxy.sock')
    env={'XDG_CONFIG_HOME':str(ROOT/'ui-config'),'XDG_STATE_HOME':str(ROOT/'ui-state'),'XDG_CACHE_HOME':str(ROOT/'ui-cache'),'FRACTAL_BAKEOFF_LOG':str(ROOT/'fractal.jsonl'),'OTEL_EXPORTER_OTLP_ENDPOINT':'http://127.0.0.1:43179','OTEL_EXPORTER_OTLP_PROTOCOL':'http/protobuf','OTEL_TRACES_SAMPLER':'always_on','OTEL_RESOURCE_ATTRIBUTES':'service.instance.id=status-bakeoff-'+CASE+'-'+HOST,'OTEL_BSP_SCHEDULE_DELAY':'500'}
    variant=CASE.split('-')[0]
    if variant=='ws':env['FRACTAL_BAKEOFF_TRANSPORT']='ws'
    result['ui']=launch('ui',[ART/('fractal-'+variant),'--st-socket',ROOT/'proxy.sock','--pty-root',ROOT/'pty','--glyphs','unicode'],env)
    if b'fractal-' not in Path(f"/proc/{result['ui']['pid']}/cmdline").read_bytes():raise RuntimeError('foreign UI process')
    terminal=os.open(f"/proc/{result['ui']['pid']}/fd/0",os.O_RDWR|os.O_NOCTTY)
    try:fcntl.ioctl(terminal,termios.TIOCSWINSZ,struct.pack('HHHH',40,160,0,0))
    finally:os.close(terminal)
    subprocess.run(['pty','send',result['ui']['session'],'--with-delay','0.1','--seq','/','--seq','scratch'],check=True)
(ROOT/('launch-'+MODE+'.json')).write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result),flush=True)
