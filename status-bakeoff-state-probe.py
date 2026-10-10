import asyncio,json,sys,time
from pathlib import Path
async def main():
    results=[]
    for path in ['/v1/claims?limit=200','/v1/client/hosts?limit=100','/v1/client/agents?fresh=true&limit=100']:
        r,w=await asyncio.open_unix_connection(sys.argv[1]);w.write(('GET '+path+' HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n').encode());await w.drain();raw=await r.read();w.close();await w.wait_closed();h,b=raw.split(b'\r\n\r\n',1)
        if b'transfer-encoding: chunked' in h.lower():
            chunks=[]
            while b:
                size,remaining=b.split(b'\r\n',1);n=int(size.split(b';')[0],16)
                if not n:break
                chunks.append(remaining[:n]);b=remaining[n+2:]
            b=b''.join(chunks)
        try:value=json.loads(b)
        except json.JSONDecodeError:value={'raw':b.decode(errors='replace')}
        results.append({'path':path,'wall_ms':time.time_ns()/1e6,'status':h.split(b'\r\n')[0].decode(),'response':value})
    Path(sys.argv[2]).write_text(json.dumps(results,indent=2)+'\n')
asyncio.run(main())
