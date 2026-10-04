// Test-only omp host for the scratch admission launch. Load the candidate's real extensions,
// consume its native channel message, and answer the isolated loopback model's tool call.
import fs from 'node:fs';
import readline from 'node:readline';
import { pathToFileURL } from 'node:url';

export async function admit() {
  const events = new Map();
  let idle = true;
  const sessionId = `admission-stub-${process.pid}`;
  const ctx = {
    isIdle: () => idle,
    sessionManager: { getSessionId: () => sessionId, getEntries: () => [] },
    ui: { notify: () => {} },
  };
  const emit = async (name, event = {}) => {
    for (const callback of events.get(name) ?? []) await callback(event, ctx);
  };
  const endpoint = JSON.parse(fs.readFileSync(`${process.env.PI_CODING_AGENT_DIR}/models.json`))
    .providers.admission.baseUrl;
  const completion = async (messages) => {
    const response = await fetch(`${endpoint}/chat/completions`, {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ messages }),
    });
    if (!response.ok) throw new Error(`fixture model: ${response.status}`);
    const deltas = (await response.text()).split('\n').filter(line => line.startsWith('data: ')
      && line !== 'data: [DONE]').map(line => JSON.parse(line.slice(6)).choices?.[0]?.delta ?? {});
    return {
      text: deltas.map(delta => delta.content ?? '').join(''),
      call: deltas.flatMap(delta => delta.tool_calls ?? [])[0],
    };
  };
  const input = readline.createInterface({ input: process.stdin });
  let approval;
  input.on('line', line => {
    const response = JSON.parse(line);
    if (response.type === 'extension_ui_response' && response.id === 'fixture-approval') {
      approval?.(response.value);
    }
  });
  const turn = async (content) => {
    idle = false;
    await emit('agent_start');
    await emit('turn_start');
    const user = { role: 'user', content: [{ type: 'text', text: content }] };
    await emit('message_start', { message: user });
    await emit('message_end', { message: user });
    const messages = [{ role: 'user', content }];
    const first = await completion(messages);
    if (first.call?.function?.name !== 'admission_fixture') throw new Error('unexpected fixture tool');
    const ask = { sessionId, toolName: first.call.function.name,
      toolCallId: first.call.id, approvalMode: 'always-ask' };
    await emit('tool_approval_requested', ask);
    const answer = new Promise(resolve => { approval = resolve; });
    console.log(JSON.stringify({ type: 'extension_ui_request', method: 'select',
      title: 'Allow tool: admission_fixture', id: 'fixture-approval' }));
    if (await answer !== 'Deny') throw new Error('admission fixture must be denied');
    await emit('tool_approval_resolved', { ...ask, approved: false });
    messages.push({ role: 'tool', tool_call_id: first.call.id, content: 'fixture denied' });
    const last = await completion(messages);
    const assistant = { role: 'assistant', stopReason: 'stop', content: [{ type: 'text', text: last.text }] };
    await emit('message_start', { message: assistant });
    await emit('message_end', { message: assistant });
    await emit('turn_end');
    await emit('agent_end', { messages: [assistant], willContinue: false });
    setTimeout(() => { idle = true; }, 30);
  };
  const api = {
    on: (name, callback) => events.set(name, [...(events.get(name) ?? []), callback]),
    registerTool: () => {}, sendMessage: () => {}, setSessionName: () => {},
    sendUserMessage: (content) => {
      setTimeout(() => turn(content).catch(error => { console.error(error); process.exit(1); }), 0);
    },
  };
  for (let i = 2; i < process.argv.length; i++) {
    if (process.argv[i] === '-e' || process.argv[i] === '--extension') {
      const { default: extension } = await import(pathToFileURL(process.argv[++i]));
      extension(api);
    }
  }
  await emit('session_start');
  setInterval(() => {}, 1000);
}
