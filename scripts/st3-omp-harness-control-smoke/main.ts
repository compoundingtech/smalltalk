import { resolve } from 'node:path';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
const binary=process.env.OMP_NATIVE_BIN;
if(!binary)throw Error('Set OMP_NATIVE_BIN to the native OMP executable (not a managed-seat launcher)');
const profile=await mkdtemp(resolve(tmpdir(),'omp-native-control-'));
const server=Bun.serve({port:0,hostname:'127.0.0.1',fetch:async request=>{
  const body=await request.json();
  if(!body.stream)return Response.json({id:'smoke',object:'chat.completion',created:1,model:'native-smoke',choices:[{index:0,message:{role:'assistant',content:'native smoke complete'},finish_reason:'stop'}],usage:{prompt_tokens:1,completion_tokens:1,total_tokens:2}});
  const chunk={id:'smoke',object:'chat.completion.chunk',created:1,model:'native-smoke',choices:[{index:0,delta:{role:'assistant',content:'native smoke complete'},finish_reason:null}]};
  const end={...chunk,choices:[{index:0,delta:{},finish_reason:'stop'}],usage:{prompt_tokens:1,completion_tokens:1,total_tokens:2}};
  return new Response(`data: ${JSON.stringify(chunk)}\n\ndata: ${JSON.stringify(end)}\n\ndata: [DONE]\n\n`,{headers:{'content-type':'text/event-stream'}});
}});
const root=resolve(import.meta.dir,'../..');
const proc=Bun.spawn([binary,'--mode','rpc','--no-ui','--no-tools','--no-lsp','--no-extensions','--no-skills','--no-rules','--no-title','--no-session','--extension',resolve(import.meta.dir,'extension.ts'),'--model','control-smoke/native-initial','--thinking','low'],{cwd:root,env:{...process.env,PI_CODING_AGENT_DIR:profile,SMOKE_BASE_URL:`http://127.0.0.1:${server.port}/v1`},stdin:'pipe',stdout:'pipe',stderr:'pipe'});
let result='';let success=false;
const deadline=setTimeout(()=>proc.kill(),30000);
const stderr=new Response(proc.stderr).text();
proc.stdin.write(JSON.stringify({type:'prompt',message:'/exercise-control',id:'exercise'})+'\n');proc.stdin.flush();
let stopping=false;
for await(const bytes of proc.stdout){const text=new TextDecoder().decode(bytes);result+=text;process.stdout.write(text);if(!stopping&&result.includes('CONTROL_SHUTDOWN_READY')){stopping=true;proc.stdin.write(JSON.stringify({type:'get_state',id:'shutdown-drain'})+'\n');proc.stdin.flush();}if(result.includes('CONTROL_SUCCESS'))success=true;}
const exitCode=await proc.exited;console.log(await stderr);server.stop();await rm(profile,{recursive:true,force:true});clearTimeout(deadline);if(exitCode!==0||!success)throw Error(`Native control smoke failed (exit ${exitCode})`);
