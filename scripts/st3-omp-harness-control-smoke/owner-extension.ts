import childProcess from 'node:child_process';
import { createHarnessControl } from '../../crates/st3/hooks/omp-harness-control.ts';

export default function(pi) {
  pi.registerProvider('control-smoke', {baseUrl:process.env.SMOKE_BASE_URL,apiKey:'test-only',api:'openai-completions',models:['native-initial','native-smoke'].map(id=>({id,name:id,reasoning:true,thinking:{efforts:['low','high'],defaultLevel:'low'},input:['text'],cost:{input:0,output:0,cacheRead:0,cacheWrite:0},contextWindow:32000,maxTokens:1000}))});
  let child, context, queued=[]; let buffer='';
  let lostReceipt, restartPending=false, replayPending=false, replayFrames;
  const send=frame=>{
    console.log('OWNER_NATIVE '+JSON.stringify(frame));
    if(frame.type==='harness_control_receipt'&&frame.status==='applied'&&!lostReceipt){
      // The native effect already happened; lose only its first driver-pipe delivery.
      lostReceipt=frame.operation_id;restartPending=true;replayPending=true;
      console.log('OWNER_NATIVE_PIPE_LOSS '+lostReceipt);
      const previous=child;child=undefined;previous.kill('SIGKILL');return;
    }
    if(replayFrames){replayFrames.push(frame);return;}
    if(child)child.stdin.write(JSON.stringify(frame)+'\n');else queued.push(frame);
  };
  const control=createHarnessControl(pi,send);
  const startDriver=()=>{
    buffer='';
    child=childProcess.spawn(process.env.ST_SMOKE_BIN,['--endpoint',process.env.SMOKE_SOCKET,'--catalog',process.env.SMOKE_ROOT,'driver','omp-channel','--identity','queue-smoke.control-smoke'],{stdio:['pipe','pipe','inherit'],env:{...process.env,ST_AGENT:'agent/queue-smoke.control-smoke',ST_DRIVER_ROOT:process.env.SMOKE_ROOT,ST_DRIVER_AGENT_DIR:process.env.SMOKE_ROOT+'/agent',ST_DRIVER_SESSION_DIR:process.env.SMOKE_ROOT+'/sessions',ST_DRIVER_IDENTITY:'queue-smoke.control-smoke',ST_OMP_CHANNEL_SESSION:'channel-smoke',ST_OMP_CHANNEL_SEQ:'1',ST_OMP_CHANNEL_RUNTIME_ID:'native-smoke'}});
    child.stdout.on('data',data=>{
      buffer+=data.toString();
      while(buffer.includes('\n')){
        const n=buffer.indexOf('\n'),line=buffer.slice(0,n);buffer=buffer.slice(n+1);
        if(!line)continue;
        const frame=JSON.parse(line);
        if(frame.type==='hello'&&replayPending){
          // One pipe write exercises baseline-before-receipt ordering in one input chunk.
          replayFrames=[];control.replay();
          child.stdin.write(replayFrames.map(frame=>JSON.stringify(frame)).join('\n')+'\n');
          if(replayFrames.some(frame=>frame.type==='harness_control_receipt'&&frame.operation_id===lostReceipt))console.log('OWNER_CONTROL_REPLAY_RECEIPT '+lostReceipt);
          replayFrames=undefined;replayPending=false;
        }
        if(frame.type==='harness_control'&&frame.command.type==='input'&&frame.command.content==='must not resend'){
          console.log('OWNER_CONTROL_HELD '+frame.command.operation_id);setTimeout(()=>control.handle(frame,context),2000);
        }else control.handle(frame,context);
        if(frame.type==='harness_control')console.log('OWNER_CONTROL_COMMAND '+frame.command.operation_id);
      }
    });
    child.on('exit',(code,signal)=>{console.log('OWNER_DRIVER_EXIT '+JSON.stringify({code,signal}));if(restartPending){restartPending=false;startDriver();}});
    for(const frame of queued)child.stdin.write(JSON.stringify(frame)+'\n');queued=[];
    send({type:'session',sessionId:context.sessionManager.getSessionId()});
    if(context.isIdle())send({type:'state',state:'idle'});
  };
  pi.on('session_start',(_event,ctx)=>{context=ctx;startDriver();});
  pi.on('agent_start',(_event,ctx)=>{context=ctx;send({type:'state',state:'active'});});
  const timer=setInterval(()=>{control.observe();if(context?.isIdle())send({type:'state',state:'idle'});},100);
  pi.on('agent_end',(_event,ctx)=>{context=ctx;});
  pi.on('session_shutdown',()=>{clearInterval(timer);child?.stdin.end();});
}
