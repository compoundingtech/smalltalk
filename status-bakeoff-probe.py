import asyncio,json,time,sys
SOCKET='/tmp/status-bakeoff-dev3/st.sock'
async def request(path,body=None):
    reader,writer=await asyncio.open_unix_connection(SOCKET)
    data=json.dumps(body).encode() if body is not None else b''
    writer.write((f'{"POST" if body is not None else "GET"} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {len(data)}\r\n\r\n').encode()+data)
    await writer.drain();raw=await asyncio.wait_for(reader.read(),30);writer.close();await writer.wait_closed()
    print(json.dumps({'at':time.time(),'path':path,'response':raw.decode()}),flush=True)
async def main():
    for kind,fields in [('runtime.observed',{'status':'running','runtime_id':'status-bakeoff-scratch','incarnation_id':'one'}),('harness.observed',{'state':'idle','driver':'codex','incarnation_id':'one'})]:
        await request('/v1/claims',{'subject':'agent/status-bakeoff/scratch','kind':kind,'actor':'agent/status-bakeoff/scratch','fields':fields,'evidence':[]})
    await request('/v1/client/agents?limit=100')
asyncio.run(main())
