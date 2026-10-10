"""Observe exact daemon publications independently of Fractal transport; no roster synthesis."""
import asyncio,datetime,hmac,json,os,time,sys
from pathlib import Path
from urllib.parse import urlencode
SOCKET,OUT=sys.argv[1:3]
async def main():
    Path(OUT).parent.mkdir(parents=True,exist_ok=True);log=open(OUT,'a',buffering=1);epoch=None;revision=0
    latest=None;changed=asyncio.Condition();ack_server=None
    if os.environ.get('BAKEOFF_ACK_BIND'):
        secret=Path(os.environ['BAKEOFF_ACK_SECRET_FILE']).read_text().strip()
        async def acknowledge(reader,writer):
            try:
                request=json.loads(await asyncio.wait_for(reader.readline(),5))
                if not hmac.compare_digest(request.get('secret',''),secret):return
                async with changed:
                    while True:
                        resource=next((a for a in (latest or {}).get('items',[]) if a.get('id')=='agent/status-bakeoff/scratch'),None)
                        if resource and resource.get('incarnation_id')==request['incarnation'] and resource.get('since'):
                            since=round(datetime.datetime.fromisoformat(resource['since'].replace('Z','+00:00')).timestamp()*1000)
                            if since==request['observed_at_ms']:break
                        await changed.wait()
                    writer.write((json.dumps({'publication':latest['publication'],'resource':resource})+'\n').encode());await writer.drain()
            finally:
                writer.close();await writer.wait_closed()
        host,port=os.environ['BAKEOFF_ACK_BIND'].rsplit(':',1)
        ack_server=await asyncio.start_server(acknowledge,host,int(port),limit=4096)
    try:
        while True:
            query={'after_revision':revision,'wait_ms':25000}
            if epoch:query['node_epoch']=epoch
            reader,writer=await asyncio.open_unix_connection(SOCKET)
            writer.write(('GET /v1/client/agents/poll?'+urlencode(query)+' HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n').encode());await writer.drain()
            raw=await asyncio.wait_for(reader.read(),35);writer.close();await writer.wait_closed();header,body=raw.split(b'\r\n\r\n',1);envelope=json.loads(body)
            log.write(json.dumps({'received_wall_ns':time.time_ns(),'received_mono_ns':time.monotonic_ns(),'status':header.split(b'\r\n')[0].decode(),'envelope':envelope},separators=(',',':'))+'\n')
            if not header.startswith(b'HTTP/1.1 200'):raise RuntimeError(envelope)
            value=envelope['value']
            if value['kind']=='resync':epoch=value['node_epoch'];revision=0
            elif value['kind']=='snapshot':
                epoch=value['publication']['node_epoch'];revision=value['publication']['revision']
                async with changed:latest=value;changed.notify_all()
            elif value['kind']!='unchanged':raise RuntimeError(value)
    finally:
        if ack_server:ack_server.close();await ack_server.wait_closed()
        log.close()
asyncio.run(main())
