"""Controlled native API sequence; runtime seed uses the scratch-only Store driver."""
import asyncio,ctypes,json,time,sys,os,select,signal
from pathlib import Path
SOCKET,INJECTOR,DB,NODE,OUT=sys.argv[1:6]
PROXY_PID=int(sys.argv[6]) if len(sys.argv)>6 and sys.argv[6].isdigit() else None
async def main():
    Path(OUT).parent.mkdir(parents=True,exist_ok=True)
    log=open(OUT,'a',buffering=1)
    def emit(**v):log.write(json.dumps({'wall_ms':time.time_ns()/1e6,'mono_ns':time.monotonic_ns(),**v},separators=(',',':'))+'\n')
    def signal_proxy(sig):
        if PROXY_PID is None:raise RuntimeError('local owned proxy PID required')
        cmdline=Path(f'/proc/{PROXY_PID}/cmdline').read_bytes()
        if b'status-bakeoff-wire.py' not in cmdline:raise RuntimeError('refusing signal to a foreign process')
        os.kill(PROXY_PID,sig)
    def await_held_view():
        wire=Path(os.environ['BAKEOFF_WIRE_LOG']);libc=ctypes.CDLL(None,use_errno=True)
        fd=libc.inotify_init1(os.O_NONBLOCK|os.O_CLOEXEC)
        if fd<0:raise OSError(ctypes.get_errno(),'inotify_init1')
        try:
            if libc.inotify_add_watch(fd,str(wire).encode(),0x2)<0:raise OSError(ctypes.get_errno(),'watch wire log')
            end=time.monotonic()+15
            while not any(json.loads(line).get('kind')=='delayed_view' for line in wire.read_text().splitlines()):
                remaining=end-time.monotonic()
                if remaining<=0 or not select.select([fd],[],[],remaining)[0]:raise TimeoutError('delayed response was never exercised')
                os.read(fd,65536)
        finally:os.close(fd)
    injector=await asyncio.create_subprocess_exec(INJECTOR,DB,NODE,stdin=asyncio.subprocess.PIPE,stdout=asyncio.subprocess.PIPE,stderr=asyncio.subprocess.PIPE)
    async def inject(kind,incarnation,sequence,fields,case):
        value={'kind':kind,'incarnation':incarnation,'sequence':sequence,'fields':fields,'case':case}
        if kind=='runtime':
            injector.stdin.write((json.dumps(value)+'\n').encode());await injector.stdin.drain()
            receipt=await asyncio.wait_for(injector.stdout.readline(),30)
            if not receipt:raise RuntimeError('injector exited: '+(await injector.stderr.read()).decode())
            decoded=json.loads(receipt)
        else:
            started=time.time_ns()//1000000
            claim={'subject':'agent/status-bakeoff/scratch','kind':'harness.observed','actor':'agent/status-bakeoff/scratch','fields':dict(fields,driver='codex',incarnation_id=incarnation,observed_at_ms=started),'evidence':[],'idempotency_key':f'bakeoff-{incarnation}-{sequence}'}
            payload=json.dumps({'runtime_incarnation':incarnation,'sequence':sequence,'claim':claim}).encode()
            reader,writer=await asyncio.open_unix_connection(SOCKET)
            writer.write(b'POST /v1/harness-events HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: '+str(len(payload)).encode()+b'\r\n\r\n'+payload);await writer.drain()
            raw=await asyncio.wait_for(reader.read(),30);writer.close();await writer.wait_closed()
            header,body=raw.split(b'\r\n\r\n',1);response=json.loads(body);record=response.get('value',response)
            if b' 200 ' in header.split(b'\r\n')[0]:
                decoded={'kind':'accepted','input':value,'started_ms':started,'accepted_ms':time.time_ns()//1000000,'record':record,'response':response,'store_index':record['store_index'],'local_frontier':0}
            else:decoded={'kind':'rejected','input':value,'started_ms':started,'finished_ms':time.time_ns()//1000000,'error':record,'http_status':header.split(b'\r\n')[0].decode()}
        emit(kind='input',case=case,receipt=decoded)
        if decoded['kind']=='accepted' and kind=='status' and case!='continuation' and os.environ.get('BAKEOFF_ACK_ADDRESS'):
            host,port=os.environ['BAKEOFF_ACK_ADDRESS'].rsplit(':',1)
            reader,writer=await asyncio.open_connection(host,int(port))
            request={'secret':Path(os.environ['BAKEOFF_ACK_SECRET_FILE']).read_text().strip(),'incarnation':incarnation,'observed_at_ms':decoded['record']['body']['fields']['observed_at_ms']}
            writer.write((json.dumps(request)+'\n').encode());await writer.drain()
            acknowledgement=json.loads(await asyncio.wait_for(reader.readline(),120))
            writer.close();await writer.wait_closed();emit(kind='downstream_materialized_ack',case=case,acknowledgement=acknowledgement)
        return decoded
    async def refresh():
        reader,writer=await asyncio.open_unix_connection(SOCKET)
        writer.write(b'GET /v1/client/agents?limit=100&fresh=true HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n');await writer.drain()
        raw=await asyncio.wait_for(reader.read(),30);writer.close();await writer.wait_closed()
        header,body=raw.split(b'\r\n\r\n',1)
        emit(kind='refresh',status=header.split(b'\r\n')[0].decode(),body=body.decode())
    try:
        await inject('runtime','one',0,{'status':'running','runtime_id':'status-bakeoff-fixture','terminal':True},'seed-runtime')
        await inject('status','one',1,{'state':'idle','blocked_on':None,'ask':None},'seed-idle');await refresh();await asyncio.sleep(2)
        if os.environ.get('BAKEOFF_RACE_ONLY'):
            signal_proxy(signal.SIGUSR2);emit(kind='race_armed')
            for sequence,(case,fields,pause) in enumerate([
                ('delayed-old-working',{'state':'working','blocked_on':None,'ask':None},0),
                ('newer-idle',{'state':'idle','blocked_on':None,'ask':None},1.4),
                ('final-human-ask',{'state':'working','blocked_on':'human','ask':'permission','reason':'Scratch race approval'},10)
            ],start=2):
                await inject('status','one',sequence,fields,case);await refresh()
                if case=='delayed-old-working':await asyncio.to_thread(await_held_view)
                await asyncio.sleep(pause)
            emit(kind='race_quiet_complete');return
        seq=1
        sequence=[('idle-to-working',{'state':'working','blocked_on':None,'ask':None}),('working-to-idle',{'state':'idle','blocked_on':None,'ask':None}),('continuation',{'state':'working','blocked_on':None,'ask':None}),('human-ask',{'state':'working','blocked_on':'human','ask':'permission','reason':'Scratch bakeoff approval'}),('human-approval',{'state':'working','blocked_on':None,'ask':None}),('quiet-last-idle',{'state':'idle','blocked_on':None,'ask':None})]
        cadence=[2.2,2.7,2.4,3.1,2.3,2.9]
        for repetition in range(int(os.environ.get('BAKEOFF_REPETITIONS','10'))):
            for j,(case,fields) in enumerate(sequence):
                if case=='continuation':
                    seq+=1;preparation=await inject('status','one',seq,fields,'continuation-start')
                    if preparation['kind']!='accepted':raise RuntimeError('continuation preparation rejected '+json.dumps(preparation))
                    await refresh();await asyncio.sleep(cadence[(j+repetition)%len(cadence)])
                seq+=1;receipt=await inject('status','one',seq,fields,case)
                if receipt['kind']!='accepted':raise RuntimeError('fixture rejected '+json.dumps(receipt))
                await refresh();await asyncio.sleep(cadence[(j+repetition)%len(cadence)])
        emit(kind='quiet_begin');await asyncio.sleep(10);emit(kind='quiet_end')
        if PROXY_PID:
            signal_proxy(signal.SIGUSR1);emit(kind='forced_disconnect')
            seq+=1;await inject('status','one',seq,{'state':'working','blocked_on':None,'ask':None},'reconnect-working');await refresh();await asyncio.sleep(4)
        await inject('runtime','two',0,{'status':'running','runtime_id':'status-bakeoff-fixture','terminal':True},'new-incarnation')
        await inject('status','two',1,{'state':'working','blocked_on':None,'ask':None},'new-incarnation-working');await refresh();await asyncio.sleep(3)
        seq+=1;replay=await inject('status','one',seq,{'state':'idle','blocked_on':None,'ask':None},'old-incarnation-replay');await refresh();await asyncio.sleep(3)
        emit(kind='replay_outcome',receipt=replay)
        await inject('status','two',2,{'state':'idle','blocked_on':None,'ask':None},'final-idle');await refresh();emit(kind='final_quiet_begin');await asyncio.sleep(10);emit(kind='final_quiet_end')
    finally:
        injector.stdin.close();await injector.wait();emit(kind='injector_exit',code=injector.returncode,stderr=(await injector.stderr.read()).decode());log.close()
asyncio.run(main())
