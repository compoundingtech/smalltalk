"""Execute owned scratch cases sequentially; never signal a foreign PID."""
import json,os,subprocess,sys
from pathlib import Path
REMOTE='/home/schickling/.megarepo/github.com/compoundingtech/smalltalk/refs/heads/schickling-assistant/2026-10-10-status-bakeoff-st'
OUT=Path('/tmp/fractal-redo/status-bakeoff');OUT.mkdir(parents=True,exist_ok=True)
def execute(host,args,env=None,capture=False):
    if host=='dev3':args=['ssh','dev3','env',*[k+'='+str(v) for k,v in (env or {}).items()],*map(str,args)];env=None
    result=subprocess.run(list(map(str,args)),env=dict(os.environ,**(env or {})),text=True,capture_output=capture,check=True)
    if capture:return result.stdout

def helper(host,name):return (REMOTE if host=='dev3' else '/tmp')+'/status-bakeoff-'+name+'.py'
def readjson(host,path):
    if host=='dev5':return json.loads(Path(path).read_text())
    # scp receipts, not shell paging or reads.
    local=OUT/('receipt-'+Path(path).parent.name+'-'+Path(path).name)
    execute('dev5',['scp','dev3:'+str(path),local]);return json.loads(local.read_text())
def root(case,host):return f'/tmp/status-bakeoff-{case}-{host}'
def copy_case(case,host):
    destination=OUT/(case+'-'+host);destination.mkdir(exist_ok=True)
    if host=='dev3':execute('dev5',['scp','dev3:'+root(case,host)+'/*.json*',destination])
    else:
        import shutil
        for receipt in Path(root(case,host)).glob('*.json*'):shutil.copy2(receipt,destination/receipt.name)
    return destination

def stop(case,host,parts):
    for part in parts:
        session=f'status-bakeoff.{case}.{host}.{part}'
        execute(host,['pty','kill',session]);execute(host,['pty','rm',session])
    if host=='dev3':
        env={'PTY_ROOT':root(case,host)+'/pty','PTY_SESSION_DIR':root(case,host)+'/pty'}
        execute(host,['pty','kill','status-bakeoff-fixture'],env);execute(host,['pty','rm','status-bakeoff-fixture'],env)

def run(case):
    cross='cross' in case;race='race' in case;uihost='dev5' if cross else 'dev3';uiroot=root(case,uihost);originroot=root(case,'dev3')
    execute('dev3',['python3',helper('dev3','launch'),case,'dev3','server' if cross else 'all'])
    if cross:execute('dev5',['python3',helper('dev5','launch'),case,'dev5','all'])
    ui=readjson(uihost,uiroot+'/launch-all.json');origin=readjson('dev3',originroot+('/launch-server.json' if cross else '/launch-all.json'))
    pids=[ui[p]['pid'] for p in ['ui','daemon','proxy','observer']]
    if cross:pids.append(ui['replication']['pid'])
    execute(uihost,['python3',helper(uihost,'cpu'),'before',uiroot+'/cpu.jsonl',*pids])
    if cross:execute('dev3',['python3',helper('dev3','cpu'),'before',originroot+'/cpu-origin.jsonl',origin['daemon']['pid'],origin['observer']['pid'],origin['replication']['pid']])
    env={'BAKEOFF_REPETITIONS':'5','ST_AGENT':'agent/status-bakeoff/scratch'}
    if cross:env.update(BAKEOFF_ACK_ADDRESS='100.124.235.29:33174',BAKEOFF_ACK_SECRET_FILE='/tmp/status-bakeoff-dev3/fleet-secret')
    if race:env.update(BAKEOFF_RACE_ONLY='1',BAKEOFF_WIRE_LOG=uiroot+'/wire.jsonl')
    command=['python3',helper('dev3','run'),originroot+'/st.sock','/tmp/status-bakeoff-artifacts/status_bakeoff_inject',originroot+'/state/claims.sqlite3','status-bakeoff-dev3',originroot+'/inputs.jsonl']
    if not cross:command.append(ui['proxy']['pid'])
    command+=['driver','codex']
    execute('dev3',command,env)
    if cross:
        # Reconnect only this dev5 Python proxy, never send its PID to dev3.
        import signal,time
        proxy=ui['proxy']['pid']
        if b'status-bakeoff-wire.py' not in Path(f'/proc/{proxy}/cmdline').read_bytes():raise RuntimeError('foreign proxy')
        os.kill(proxy,signal.SIGUSR1);time.sleep(5)
    execute(uihost,['python3',helper(uihost,'cpu'),'after',uiroot+'/cpu.jsonl',*pids])
    if cross:execute('dev3',['python3',helper('dev3','cpu'),'after',originroot+'/cpu-origin.jsonl',origin['daemon']['pid'],origin['observer']['pid'],origin['replication']['pid']])
    # PTY peek documents the actual terminal independently of buffer instrumentation.
    screen=execute(uihost,['pty','peek','--plain',ui['ui']['session']],capture=True);(OUT/(case+'-screen.txt')).write_text(screen)
    destination=copy_case(case,uihost);source=copy_case(case,'dev3') if cross else destination
    if race:
        execute('dev5',['python3',helper('dev5','race-analyze'),source/'inputs.jsonl',destination/'fractal.jsonl',destination/'wire.jsonl',OUT/(case+'-race.json')])
    else:
        args=['python3',helper('dev5','analyze'),source/'inputs.jsonl',destination/'fractal.jsonl',destination/'wire.jsonl',destination/'cpu.jsonl',OUT/(case+'-summary.json'),destination/'publications.jsonl']
        if cross:args.append(source/'publications.jsonl')
        execute('dev5',args,{'BAKEOFF_CROSS_HOST':'1'} if cross else {})
    stop(case,uihost,['ui','proxy','observer']+(['replication'] if cross else [])+['daemon'])
    if cross:stop(case,'dev3',['observer','replication','daemon'])
    print('COMPLETED '+case,flush=True)
for case in sys.argv[1:]:run(case)
