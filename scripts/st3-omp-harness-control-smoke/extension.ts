import { createHarnessControl } from '../../crates/st3/hooks/omp-harness-control.ts';
export default function(pi) {
  pi.registerProvider('control-smoke', { baseUrl: process.env.SMOKE_BASE_URL, apiKey:'test-only', api:'openai-completions', models:[{id:'native-smoke',name:'Native Smoke',reasoning:true,thinking:{efforts:['low','high']},input:['text'],cost:{input:0,output:0,cacheRead:0,cacheWrite:0},contextWindow:32000,maxTokens:1000}] });
  let frames=[]; let losing=false;
  let shutdownBinding;
  let seedObserved=false;
  pi.on('message_end',event=>{if(event.message.role==='user')seedObserved=true;});
  pi.on('session_shutdown',(_event,ctx)=>{if(shutdownBinding)control.handle({type:'harness_control',command:{type:'set_model',operation_id:'shutdown-model',binding:shutdownBinding,provider:'control-smoke',model_id:'native-smoke',effort:'low'}},ctx);});
  const control=createHarnessControl(pi,frame=>{if(!losing) frames.push(frame);console.log('CONTROL_PROOF '+JSON.stringify(frame));});
  pi.on('session_shutdown',()=>{if(!shutdownBinding)return;const receipt=frames.find(f=>f.type==='harness_control_receipt'&&f.operation_id==='shutdown-model');if(receipt?.status!=='indeterminate')throw Error('Pending native model shutdown not indeterminate');console.log('CONTROL_SUCCESS '+JSON.stringify({identical_text_ids:['input-a','input-b'],switch_during_admission:'cancelled',model_effort:'observed',pending_model_shutdown:'indeterminate',approval:frames.find(f=>f.type==='harness_control_state')?.approval}));});
  pi.registerCommand('exercise-control',{handler:async(_args,ctx)=>{
    const pause=()=>Bun.sleep(10);
    const awaitReceipt=async(id)=>{for(let i=0;i<500;i++){const r=frames.find(f=>f.type==='harness_control_receipt'&&f.operation_id===id);if(r)return r;await pause();}throw Error('Missing receipt '+id);};
    const bind=()=>({desired_revision:'revision',incarnation_id:'incarnation',session_id:ctx.sessionManager.getSessionId(),turn_id:frames.filter(f=>f.type==='harness_control_state').at(-1)?.turn_id??null});
    control.handle({type:'harness_control_binding',binding:bind()},ctx);
    let binding=bind();
    control.handle({type:'harness_control',command:{type:'set_model',operation_id:'model-off',binding,provider:'control-smoke',model_id:'native-smoke',effort:'off'}},ctx);
    if((await awaitReceipt('model-off')).result?.effective_effort!=='off')throw Error('Native off effort not observed');
    control.handle({type:'harness_control',command:{type:'set_model',operation_id:'model',binding,provider:'control-smoke',model_id:'native-smoke',effort:'high'}},ctx);
    if((await awaitReceipt('model')).status!=='applied')throw Error('Model not applied');
    control.handle({type:'harness_control',command:{type:'set_model',operation_id:'bad-effort',binding:bind(),provider:'control-smoke',model_id:'native-smoke',effort:'adaptive'}},ctx);
    if((await awaitReceipt('bad-effort')).status!=='rejected')throw Error('Unknown effort not rejected');
    control.handle({type:'harness_control',command:{type:'input',operation_id:'stale',entry_id:'stale',actor:'person',content:'same text',lane:'steer',binding:{...bind(),session_id:'other-session'}}},ctx);
    if((await awaitReceipt('stale')).status!=='rejected')throw Error('Stale binding not rejected');
    control.handle({type:'harness_control',command:{type:'input',operation_id:'stale-turn',entry_id:'stale-turn',actor:'person',content:'same text',lane:'steer',binding:{...bind(),turn_id:'other-turn'}}},ctx);
    if((await awaitReceipt('stale-turn')).status!=='rejected')throw Error('Stale turn not rejected');
    pi.sendUserMessage('branch seed');
    for(let i=0;i<500&&!seedObserved;i++)await pause();
    if(!seedObserved)throw Error('Native seed event missing');
    await ctx.waitForIdle();
    const seed=ctx.sessionManager.getEntries().find(entry=>entry.type==='message'&&entry.message.role==='user');
    if(!seed)throw Error('Native branch seed entry missing');
    for(const id of ['input-a','input-b']){
      binding=bind();
      control.handle({type:'harness_control',command:{type:'input',operation_id:id,entry_id:id,actor:'person',content:'same text',lane:'follow_up',binding}},ctx);
      const transitionAttempt=id==='input-a'?await ctx.branch(seed.id):await ctx.newSession();
      if(!transitionAttempt.cancelled)throw Error('Unsafe native branch or switch was allowed');
      const receipt=await awaitReceipt(id);
      if(receipt.status!=='applied'||ctx.sessionManager.getSessionId()!==binding.session_id)throw Error('Input lost exact session');
      await ctx.waitForIdle();
    }
    const old=bind();
    frames=[];losing=true;control.replay();losing=false;control.replay();
    if((await awaitReceipt('input-a')).status!=='applied')throw Error('Receipt response loss replay failed');
    const changed=await ctx.branch(seed.id);if(changed.cancelled)throw Error('Resolved admission still fenced');
    control.handle({type:'harness_control_binding',binding:bind()},ctx);
    control.handle({type:'harness_control',command:{type:'input',operation_id:'after-reset',entry_id:'after-reset',actor:'person',content:'same text',lane:'steer',binding:old}},ctx);
    if((await awaitReceipt('after-reset')).status!=='rejected')throw Error('Reset binding not rejected');
    shutdownBinding=bind();
    console.log('CONTROL_SHUTDOWN_READY');
  }});
}
