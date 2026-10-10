"""Transparent scratch-only Unix proxy: exact wire bytes, parsed revisions, one delayed view."""
import asyncio,json,time,os,sys,struct,signal
from collections import deque
from pathlib import Path
SOURCE,LISTEN,OUT=sys.argv[1:4]
DELAY_MS=int(os.environ.get('BAKEOFF_DELAY_VIEW_MS','0'))
class Parser:
    def __init__(self):self.buf=b'';self.ws=False
    def feed(self,data):
        self.buf+=data;messages=[]
        while self.buf:
            if self.ws:
                if len(self.buf)<2:break
                a,b=self.buf[:2];n=b&127;offset=2
                if n==126:
                    if len(self.buf)<4:break
                    n=struct.unpack('!H',self.buf[2:4])[0];offset=4
                elif n==127:
                    if len(self.buf)<10:break
                    n=struct.unpack('!Q',self.buf[2:10])[0];offset=10
                masked=b&128
                if masked:offset+=4
                if len(self.buf)<offset+n:break
                payload=self.buf[offset:offset+n]
                if masked:
                    mask=self.buf[offset-4:offset];payload=bytes(v^mask[i%4] for i,v in enumerate(payload))
                self.buf=self.buf[offset+n:];opcode=a&15
                value=None
                if opcode==1:
                    try:value=json.loads(payload)
                    except (ValueError,UnicodeDecodeError):pass
                messages.append({'kind':'frame','wire_bytes':offset+n,'opcode':opcode,'value':value})
                continue
            marker=self.buf.find(b'\r\n\r\n')
            if marker<0:break
            header=self.buf[:marker];lines=header.split(b'\r\n');fields={}
            for line in lines[1:]:
                k,_,v=line.partition(b':');fields[k.lower()]=v.strip().lower()
            offset=marker+4;bodylen=int(fields.get(b'content-length',b'0'));consumed=offset+bodylen
            if fields.get(b'transfer-encoding')==b'chunked':
                cursor=offset;pieces=[];complete=False
                while True:
                    end=self.buf.find(b'\r\n',cursor)
                    if end<0:break
                    n=int(self.buf[cursor:end].split(b';')[0],16)
                    if len(self.buf)<end+2+n+2:break
                    if n==0:consumed=end+4;complete=True;break
                    pieces.append(self.buf[end+2:end+2+n]);cursor=end+2+n+2
                if not complete:break
                payload=b''.join(pieces)
            else:
                if len(self.buf)<consumed:break
                payload=self.buf[offset:consumed]
            first=lines[0].decode(errors='replace');value=None
            if payload:
                try:value=json.loads(payload)
                except (ValueError,UnicodeDecodeError):pass
            messages.append({'kind':'http','first_line':first,'wire_bytes':consumed,'value':value})
            self.buf=self.buf[consumed:]
            if fields.get(b'upgrade')==b'websocket' or first.startswith('HTTP/1.1 101'):self.ws=True
        return messages
async def main():
    Path(OUT).parent.mkdir(parents=True,exist_ok=True)
    if Path(LISTEN).exists():raise RuntimeError('refusing to replace an existing proxy socket')
    log=open(OUT,'a',buffering=1);counter=0;connections=set();delay_remaining=1 if DELAY_MS else 0
    def emit(**record):log.write(json.dumps({'wall_ns':time.time_ns(),'mono_ns':time.monotonic_ns(),**record},separators=(',',':'))+'\n')
    def disconnect():
        emit(kind='forced_disconnect',connections=len(connections))
        for writer in tuple(connections):writer.close()
    def arm_delay():
        nonlocal delay_remaining
        delay_remaining=1;emit(kind='armed_delayed_view',delay_ms=DELAY_MS or 4000)
    asyncio.get_running_loop().add_signal_handler(signal.SIGUSR1,disconnect)
    asyncio.get_running_loop().add_signal_handler(signal.SIGUSR2,arm_delay)
    async def connection(cr,cw):
        nonlocal counter
        counter+=1;cid=counter;connections.add(cw);requests=deque();ws_path=None
        sr,sw=await asyncio.open_unix_connection(SOURCE)
        async def pipe(reader,writer,direction):
            nonlocal delay_remaining,ws_path
            parser=Parser()
            while data:=await reader.read(65536):
                emit(kind='wire',connection=cid,direction=direction,bytes=len(data));held_publication=None;held_path=None
                for message in parser.feed(data):
                    if message['kind']=='http':
                        if direction=='request':
                            path=message['first_line'].split(' ')[1];requests.append(path)
                            if 'collections/stream' in path:ws_path=path
                        else:path=requests.popleft() if requests else None
                        message['path']=path
                    else:message['path']=ws_path
                    emit(connection=cid,direction=direction,**message)
                    value=message.get('value')
                    if isinstance(value,dict):
                        inner=value.get('value',value)
                        publication=value.get('publication') or (inner.get('publication') if isinstance(inner,dict) else None)
                        if publication and any(route in (message.get('path') or '') for route in ['agents/poll','collections/stream']):
                            held_publication=publication;held_path=message['path']
                if direction=='response' and delay_remaining and held_publication:
                    delay_remaining-=1;delay=DELAY_MS or 4000
                    emit(kind='delayed_view',connection=cid,delay_ms=delay,publication=held_publication,path=held_path);await asyncio.sleep(delay/1000)
                    emit(kind='released_delayed_view',connection=cid,publication=held_publication,path=held_path)
                writer.write(data);await writer.drain()
            try:writer.write_eof()
            except (OSError,AttributeError):pass
        emit(kind='open',connection=cid)
        try:await asyncio.gather(pipe(cr,sw,'request'),pipe(sr,cw,'response'))
        except (ConnectionError,asyncio.CancelledError) as error:emit(kind='closed',connection=cid,error=type(error).__name__)
        finally:
            connections.discard(cw);cw.close();sw.close();await asyncio.gather(cw.wait_closed(),sw.wait_closed(),return_exceptions=True)
    server=await asyncio.start_unix_server(connection,LISTEN);emit(kind='ready',source=SOURCE,listen=LISTEN,pid=os.getpid())
    try:
        async with server:await server.serve_forever()
    finally:server.close();await server.wait_closed();Path(LISTEN).unlink(missing_ok=True);log.close()
asyncio.run(main())
