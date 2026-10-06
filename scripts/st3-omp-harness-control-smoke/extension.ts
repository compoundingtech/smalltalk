import { createHarnessControl } from '../../crates/st3/hooks/omp-harness-control.ts';
export default function(pi) {
  pi.registerProvider('control-smoke', { baseUrl: process.env.SMOKE_BASE_URL, apiKey:'test-only', api:'openai-completions', models:['native-initial','native-smoke'].map(id=>({id,name:id,reasoning:true,thinking:{efforts:['low','high'],defaultLevel:'low'},input:['text'],cost:{input:0,output:0,cacheRead:0,cacheWrite:0},contextWindow:32000,maxTokens:1000})) });
  let frames=[]; let losing=false;
  const modelRevision=()=>frames.filter(frame=>frame.type==='harness_control_state').at(-1)?.models.revision;
  let shutdownBinding;
  let seedObserved=false;
  pi.on('message_end',event=>{if(event.message.role==='user')seedObserved=true;});
  pi.on('session_shutdown',(_event,ctx)=>{if(shutdownBinding)control.handle({type:'harness_control',command:{type:'set_model',operation_id:'shutdown-model',binding:shutdownBinding,model_revision:modelRevision(),provider:'control-smoke',model_id:'native-smoke',effort:'low'}},ctx);});
  const control=createHarnessControl(pi,frame=>{if(!losing) frames.push(frame);console.log('CONTROL_PROOF '+JSON.stringify(frame));});
  pi.on('session_shutdown',()=>{if(!shutdownBinding)return;const receipt=frames.find(f=>f.type==='harness_control_receipt'&&f.operation_id==='shutdown-model');if(receipt?.status!=='indeterminate')throw Error('Pending native model shutdown not indeterminate');console.log('CONTROL_SUCCESS '+JSON.stringify({identical_text_ids:['input-a','input-b'],switch_during_admission:'cancelled',model_effort:'observed',pending_model_shutdown:'indeterminate',approval:frames.find(f=>f.type==='harness_control_state')?.approval}));});
  pi.registerCommand('exercise-control',{handler:async(_args,ctx)=>{
    const pause=()=>Bun.sleep(10);
    const awaitReceipt=async(id)=>{for(let i=0;i<500;i++){const r=frames.find(f=>f.type==='harness_control_receipt'&&f.operation_id===id);if(r)return r;await pause();}throw Error('Missing receipt '+id);};
    const awaitNativeIdle=async()=>{
      await ctx.waitForIdle();
      // A completed turn can precede agent_end; admission requires native idle.
      for(let i=0;i<500;i++){control.observe();if(ctx.isIdle())return;await pause();}
      throw Error('Positive native idle not observed');
    };
    const bind=()=>({desired_revision:'revision',incarnation_id:'incarnation',session_id:ctx.sessionManager.getSessionId(),turn_id:frames.filter(f=>f.type==='harness_control_state').at(-1)?.turn_id??null});
    control.handle({type:'harness_control_binding',binding:bind()},ctx);
    const steer=frames.filter(frame=>frame.type==='harness_control_state').at(-1)?.steer;
    if(steer?.state!=='unsupported'||steer.reason!=='native-pre-dequeue-api-unavailable')throw Error('Native steering falsely advertised');
    control.handle({type:'harness_control',command:{type:'input',operation_id:'unsupported-steer',entry_id:'unsupported-steer',actor:'person',content:'never native input',lane:'steer',binding:bind()}},ctx);
    if((await awaitReceipt('unsupported-steer')).reason!=='native-pre-dequeue-api-unavailable')throw Error('Unsupported steering did not fail explicitly');
    const acknowledgedFrameCount=frames.length;
    for(let i=0;i<30;i++){control.handle({type:'harness_control_binding',binding:bind()},ctx);control.observe();}
    if(frames.length!==acknowledgedFrameCount)throw Error('Unchanged native ACK/keepalive emitted duplicate state');
    let binding=bind();
    control.handle({type:'harness_control',command:{type:'set_model',operation_id:'model-off',binding,model_revision:modelRevision(),provider:'control-smoke',model_id:'native-smoke',effort:'off'}},ctx);
    if((await awaitReceipt('model-off')).result?.effective_effort!=='off')throw Error('Native off effort not observed');
    const previousRevision=modelRevision();
    pi.setThinkingLevel('low');
    control.handle({type:'harness_control',command:{type:'set_model',operation_id:'stale-model',binding,model_revision:previousRevision,provider:'control-smoke',model_id:'native-smoke',effort:'high'}},ctx);
    if((await awaitReceipt('stale-model')).reason!=='stale-native-model-revision')throw Error('Native external effort change did not fence stale model revision');
    control.observe();
    control.handle({type:'harness_control',command:{type:'set_model',operation_id:'model',binding,model_revision:modelRevision(),provider:'control-smoke',model_id:'native-smoke',effort:'high'}},ctx);
    if((await awaitReceipt('model')).status!=='applied')throw Error('Model not applied');
    control.handle({type:'harness_control',command:{type:'set_model',operation_id:'model-retained',binding:bind(),model_revision:modelRevision(),provider:'control-smoke',model_id:'native-initial'}},ctx);
    const retained=await awaitReceipt('model-retained');
    if(retained.status!=='applied'||retained.result?.effective_effort!=='high')throw Error('Omitted effort replaced the live high selector with a model default');
    control.handle({type:'harness_control',command:{type:'set_model',operation_id:'bad-effort',binding:bind(),model_revision:modelRevision(),provider:'control-smoke',model_id:'native-smoke',effort:'adaptive'}},ctx);
    if((await awaitReceipt('bad-effort')).status!=='rejected')throw Error('Unknown effort not rejected');
    control.handle({type:'harness_control',command:{type:'input',operation_id:'stale',entry_id:'stale',actor:'person',content:'same text',lane:'steer',binding:{...bind(),session_id:'other-session'}}},ctx);
    if((await awaitReceipt('stale')).status!=='rejected')throw Error('Stale binding not rejected');
    control.handle({type:'harness_control',command:{type:'input',operation_id:'stale-turn',entry_id:'stale-turn',actor:'person',content:'same text',lane:'steer',binding:{...bind(),turn_id:'other-turn'}}},ctx);
    if((await awaitReceipt('stale-turn')).status!=='rejected')throw Error('Stale turn not rejected');
    pi.sendUserMessage('branch seed');
    for(let i=0;i<500&&!seedObserved;i++)await pause();
    if(!seedObserved)throw Error('Native seed event missing');
    await awaitNativeIdle();
    const seed=ctx.sessionManager.getEntries().find(entry=>entry.type==='message'&&entry.message.role==='user');
    if(!seed)throw Error('Native branch seed entry missing');
    for(const id of ['input-a','input-b']){
      binding=bind();
      control.handle({type:'harness_control',command:{type:'input',operation_id:id,entry_id:id,actor:'person',content:'same text',lane:'follow_up',binding}},ctx);
      const immediateReceipt=frames.find(f=>f.type==='harness_control_receipt'&&f.operation_id===id);
      if(immediateReceipt)throw Error('Input was not pending before transition: '+JSON.stringify(immediateReceipt));
      const transitionAttempt=id==='input-a'?await ctx.branch(seed.id):await ctx.newSession();
      if(!transitionAttempt.cancelled)throw Error('Unsafe native branch or switch was allowed');
      const receipt=await awaitReceipt(id);
      if(receipt.status!=='applied'||ctx.sessionManager.getSessionId()!==binding.session_id)throw Error('Input lost exact session');
      await awaitNativeIdle();
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
    ctx.shutdown();
  }});
}
