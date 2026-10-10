"""Real scratch daemon API contract probes; no glyph or latency SLO claims."""
import asyncio,base64,json,os,struct,time
from urllib.parse import urlencode
from pathlib import Path
SOCKET='/tmp/status-bakeoff-dev3/st.sock';DB='/tmp/status-bakeoff-dev3/state/claims.sqlite3';INJECT='/tmp/status-bakeoff-target-st/debug/examples/status_bakeoff_inject'
OUT='/tmp/status-bakeoff-dev3/api-contract.jsonl'
async def request(path):
    start=time.time_ns();reader,writer=await asyncio.open_unix_connection(SOCKET)
    writer.write(('GET '+path+' HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n').encode());await writer.drain()
    raw=await asyncio.wait_for(reader.read(),35);writer.close();await writer.wait_closed();head,body=raw.split(b'\r\n\r\n',1)
    data=json.loads(body);emit(kind='http',path=path,started_ns=start,finished_ns=time.time_ns(),status=head.split(b'\r\n')[0].decode(),data=data)
    if not head.startswith(b'HTTP/1.1 200'):raise RuntimeError(data)
    return data.get('value',data)
def emit(**v):
    with open(OUT,'a') as f:f.write(json.dumps({'wall_ns':time.time_ns(),**v})+'\n')
async def main():
    injector=await asyncio.create_subprocess_exec(INJECT,DB,'status-bakeoff-dev3',stdin=asyncio.subprocess.PIPE,stdout=asyncio.subprocess.PIPE,stderr=asyncio.subprocess.PIPE)
    async def inject(kind,seq,fields,inc='probe-one'):
        value={'kind':kind,'sequence':seq,'fields':fields,'incarnation':inc}
        injector.stdin.write((json.dumps(value)+'\n').encode());await injector.stdin.drain();line=await asyncio.wait_for(injector.stdout.readline(),30)
        if not line:raise RuntimeError((await injector.stderr.read()).decode())
        value=json.loads(line);emit(kind='input',receipt=value);return value
    ws=None
    try:
        await request('/v1/client/capabilities')
        await inject('runtime',0,{'status':'running','runtime_id':'status-bakeoff-scratch'})
        seed=await inject('status',1,{'state':'idle','blocked_on':None,'ask':None})
        first=await request('/v1/client/agents/poll?'+urlencode({'min_store_index':seed['store_index'],'wait_ms':5000}))
        assert first['kind']=='snapshot' and first['items']
        pub=first['publication'];assert pub['status_watermark']['store_index']>=seed['store_index']
        timeout=await request('/v1/client/agents/poll?'+urlencode({'node_epoch':pub['node_epoch'],'after_revision':pub['revision'],'wait_ms':200}))
        assert timeout['kind']=='unchanged' and 'items' not in timeout and 'publication' not in timeout
        resync=await request('/v1/client/agents/poll?'+urlencode({'node_epoch':'definitely-old-scratch-epoch','after_revision':pub['revision'],'wait_ms':0}))
        assert resync['kind']=='resync' and 'items' not in resync
        reader,writer=await asyncio.open_unix_connection(SOCKET);ws=writer
        key=base64.b64encode(os.urandom(16)).decode()
        writer.write(('GET /v1/client/collections/stream HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: '+key+'\r\nSec-WebSocket-Protocol: st3.client.collections.v0\r\n\r\n').encode());await writer.drain()
        header=await asyncio.wait_for(reader.readuntil(b'\r\n\r\n'),10);assert header.startswith(b'HTTP/1.1 101'),header
        async def send(value,opcode=1):
            payload=json.dumps(value).encode() if opcode==1 else value;mask=os.urandom(4);n=len(payload);size=bytes([128|n]) if n<126 else bytes([128|126])+struct.pack('!H',n)
            writer.write(bytes([128|opcode])+size+mask+bytes(v^mask[i%4] for i,v in enumerate(payload)));await writer.drain()
        async def frame():
            while True:
                a,b=await asyncio.wait_for(reader.readexactly(2),10);n=b&127
                if n==126:n=struct.unpack('!H',await reader.readexactly(2))[0]
                elif n==127:n=struct.unpack('!Q',await reader.readexactly(8))[0]
                payload=await reader.readexactly(n);opcode=a&15
                if opcode==9:await send(payload,10);continue
                if opcode!=1:raise RuntimeError(('unexpected opcode',opcode))
                value=json.loads(payload);emit(kind='ws',frame=value);return value
        await send({'kind':'subscribe','id':'agents-probe','collection':'agents','limit':200})
        initial=await frame();assert initial['kind']=='snapshot' and initial.get('publication')
        working=await inject('status',2,{'state':'working','blocked_on':None,'ask':None})
        polled=await request('/v1/client/agents/poll?'+urlencode({'node_epoch':pub['node_epoch'],'after_revision':pub['revision'],'min_store_index':working['store_index'],'wait_ms':5000}))
        assert polled['kind']=='snapshot' and polled['publication']['status_watermark']['store_index']>=working['store_index']
        assert any(a.get('harness_state')=='working' for a in polled['items'])
        streamed=await frame();assert streamed.get('publication') and streamed['publication']['revision']>=polled['publication']['revision']
        await inject('runtime',0,{'status':'running','runtime_id':'status-bakeoff-scratch'},'probe-two')
        await inject('status',1,{'state':'working','blocked_on':None,'ask':None},'probe-two')
        old=await inject('status',3,{'state':'idle','blocked_on':None,'ask':None},'probe-one')
        assert old['kind']=='rejected',old
        emit(kind='complete',cases=['full-publication','timeout-without-stale-rows','epoch-resync-without-rows','bound-ws-metadata','requested-watermark-coverage','native-old-incarnation-rejected'])
        print('API_CONTRACT_PROBES_PASS')
    finally:
        if ws:ws.close();await ws.wait_closed()
        injector.stdin.close();await injector.wait()
asyncio.run(main())
