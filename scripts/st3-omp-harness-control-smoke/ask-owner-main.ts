// Real pinned OMP TUI + production st channel + owner HTTP action. Provider responses are
// deterministic and zero-cost; neither the ask tool nor terminal/receipt transport is mocked.
import { mkdir, mkdtemp, readFile, readdir, rm } from 'node:fs/promises';
import { resolve } from 'node:path';
const st = process.env.ST_SMOKE_BIN, daemon = process.env.ST_SMOKE_DAEMON, omp = process.env.OMP_NATIVE_BIN;
if (!st || !daemon || !omp || !process.env.SMOKE_BULK_DIR) throw Error('ST_SMOKE_BIN, ST_SMOKE_DAEMON, OMP_NATIVE_BIN, SMOKE_BULK_DIR required');
await mkdir(process.env.SMOKE_BULK_DIR, { recursive: true });
const root = await mkdtemp(resolve(process.env.SMOKE_BULK_DIR, 'ask-owner-'));
const sessions = root + '/.omp/agent/sessions/ask-project';
await mkdir(root + '/agent'); await mkdir(sessions, { recursive: true });
const socket = root + '/daemon.sock', subject = 'agent/control-smoke';
const clean = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith('ST_') && !key.startsWith('ST3_') && !key.startsWith('AGENT_') && !key.startsWith('PTY_')));
const providerRequests = [];
const scenarios = [
  { id: 'native-ask-mixed', questions: [
    { id: 'color', question: 'Choose a color', options: [{ label: 'Red' }, { label: 'Blue', description: 'Cool color', preview: 'Blue preview' }], recommended: 1 },
    { id: 'extras', question: 'Choose any extras', options: [{ label: 'Alpha' }, { label: 'Beta' }], multi: true },
    { id: 'note', question: 'Provide a custom answer', options: [{ label: 'Preset' }] },
  ], answers: [
    { questionId: 'color', options: ['Red'] },
    { questionId: 'extras', options: ['Beta', 'Alpha'], text: 'Gamma extra' },
    { questionId: 'note', options: [], text: 'Custom text with a newline\nsecond line' },
  ] },
  { id: 'native-ask-single', questions: [{ id: 'single', question: 'Choose the next answer', options: [{ label: 'Keep' }, { label: 'Change' }] }], answers: [{ questionId: 'single', options: ['Change'] }] },
  { id: 'native-ask-text', questions: [{ id: 'text', question: 'Supply free text', options: [{ label: 'Preset' }] }], text: 'Literal free text' },
  { id: 'native-ask-empty-multi', questions: [{ id: 'none', question: 'Select no extras', options: [{ label: 'Alpha' }, { label: 'Beta' }], multi: true }], answers: [{ questionId: 'none', options: [] }] },
  { id: 'native-ask-edited', questions: [{ id: 'edited', question: 'Edit this in the terminal', options: [{ label: 'First' }, { label: 'Second' }] }], answers: [{ questionId: 'edited', options: ['First'] }] },
];
let scenarioIndex = 0;
const server = Bun.serve({ port: 0, hostname: '127.0.0.1', fetch: async req => {
  const body = await req.json(); providerRequests.push(body);
  const last = body.messages?.at(-1);
  const isContinuation = last?.role === 'tool';
  const scenario = scenarios[scenarioIndex];
  const delta = isContinuation ? { role: 'assistant', content: 'Native ask continuation observed.' } : { role: 'assistant', tool_calls: [{ index: 0, id: scenario.id, type: 'function', function: { name: 'ask', arguments: JSON.stringify({ questions: scenario.questions }) } }] };
  const chunk = { id: 'ask-smoke', object: 'chat.completion.chunk', created: 1, model: 'native-smoke', choices: [{ index: 0, delta, finish_reason: null }] };
  const end = { ...chunk, choices: [{ index: 0, delta: {}, finish_reason: isContinuation ? 'stop' : 'tool_calls' }], usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 } };
  return new Response(`data: ${JSON.stringify(chunk)}\n\ndata: ${JSON.stringify(end)}\n\ndata: [DONE]\n\n`, { headers: { 'content-type': 'text/event-stream' } });
} });
const nativeEnv = {
  ...clean, PI_CODING_AGENT_DIR: root + '/profile', SMOKE_ROOT: root, SMOKE_SOCKET: socket,
  SMOKE_BASE_URL: `http://127.0.0.1:${server.port}/v1`, ST3_ENDPOINT: socket, ST_DRIVER_ROOT: root,
  ST_DRIVER_AGENT_DIR: root + '/agent', ST_DRIVER_SESSION_DIR: sessions, ST_DRIVER_IDENTITY: 'control-smoke',
  ST_AGENT: subject, ST_OMP_CHANNEL_BIN: st, ST_OMP_CHANNEL_IDENTITY: 'control-smoke',
  ST_OMP_CHANNEL_RUNTIME_ID: 'native-smoke', ST_OMP_CHANNEL_SESSION: 'ask-channel', ST_OMP_CHANNEL_SEQ: '1',
};
const ptyEnv = { ...clean, PTY_ROOT: root + '/pty' };
const pty = async (...args) => {
  const process = Bun.spawn(['pty', ...args], { env: ptyEnv, stdin: 'ignore', stdout: 'pipe', stderr: 'pipe' });
  const [code, out, err] = await Promise.all([process.exited, new Response(process.stdout).text(), new Response(process.stderr).text()]);
  if (code !== 0) throw Error(`pty ${args.join(' ')}: ${err}`);
  return out;
};
const request = async (path, body, person = true) => {
  const response = await fetch('http://localhost' + path, { unix: socket, method: body ? 'POST' : 'GET', headers: { 'content-type': 'application/json', ...(person ? { 'x-st3-person': 'person/operator' } : {}) }, ...(body ? { body: JSON.stringify(body) } : {}) });
  const envelope = await response.json(); return { status: response.status, value: envelope.value, envelope };
};
const poll = async (read, label) => {
  for (let i = 0; i < (process.env.SMOKE_BROWSER_HOLD ? 72000 : 1200); i += 1) { const value = await read(); if (value) return value; await Bun.sleep(25); }
  throw Error('Timed out ' + label + '\n' + await pty('peek', '--plain', '--full', 'native-smoke'));
};
const read = async () => { const response = await request('/v1/client/harness-queue/' + encodeURIComponent(subject)); if (response.status !== 200) throw Error(JSON.stringify(response)); return response.value; };
const answer = async (key, parameters, person = true) => {
  const cap = await request('/v1/client/capabilities');
  const body = { api_version: 'st3.client.v0', id: 'action/' + key, type: 'harness.answer_ask', idempotency_key: 'native-ask-smoke-' + key, fence: { snapshot_id: cap.envelope.snapshot.id }, parameters };
  return { ...await request('/v1/client/actions', body, person), body };
};
let daemonProcess;
const deadline = setTimeout(() => { daemonProcess?.kill(); void pty('kill', 'native-smoke').catch(() => {}); }, process.env.SMOKE_BROWSER_HOLD ? 1800000 : 150000);
try {
  // Start the PTY before the owner reads its actual PID/creation incarnation. The
  // launcher waits for the owner's socket before starting OMP and its production channel.
  const command = ['bash', '-c', 'while [ ! -S "$SMOKE_SOCKET" ]; do sleep 0.05; done; exec "$@"', 'ask-launcher', omp, '--no-lsp', '--no-extensions', '--no-skills', '--no-rules', '--no-title', '--tools', 'ask', '--extension', resolve(import.meta.dir, 'ask-owner-extension.ts'), '--extension', resolve(import.meta.dir, '../../crates/st3/hooks/omp-channel.ts'), '--session-dir', sessions, '--model', 'control-smoke/native-smoke', '--thinking', 'off'];
  await pty('run', '-d', '--id', 'native-smoke', '--cwd', root, ...Object.entries(nativeEnv).flatMap(([key, value]) => ['--env', `${key}=${value}`]), '--', ...command);
  daemonProcess = Bun.spawn([daemon, root], { env: { ...clean, SMOKE_REAL_PTY: '1', ST_SMOKE_CLIENT_SOCKET: root + '/client.sock' }, stdin: 'ignore', stdout: 'pipe', stderr: 'pipe' });
  const daemonStderr = new Response(daemonProcess.stderr).text();
  await poll(async () => { try { return (await request('/v1/health')).status === 200; } catch { return false; } }, 'owner daemon');
  await poll(async () => { const q = await read(); return q.native?.input_supported && q.native.idle ? q : false; }, 'native idle binding');
  const capabilities = await request('/v1/client/capabilities');
  if (!capabilities.value.capabilities.some(cap => cap.id === 'harness.answer_ask' && cap.state === 'granted')) throw Error('Public answer action is not granted');
  const proof = [];
  let oldRef;
  for (scenarioIndex = 0; scenarioIndex < scenarios.length; scenarioIndex += 1) {
    const scenario = scenarios[scenarioIndex];
    await pty('send', 'native-smoke', '--seq', 'Start isolated ask ' + scenario.id, '--seq', 'key:return');
    const queued = await poll(async () => { const q = await read(); return q.native?.pending_ask?.tool_call_id === scenario.id ? q : false; }, 'native pending ' + scenario.id);
    const pending = queued.native.pending_ask;
    if (process.env.SMOKE_BROWSER_HOLD && scenarioIndex === 0) {
      const ready = { root, socket, client_socket: root + '/client.sock', subject, session_id: queued.native.binding.session_id, pending, parameters: { _tag: 'Selection', askRef: pending.ask_ref, answers: scenario.answers } };
      await Bun.write(root + '/browser-ready.json', JSON.stringify(ready)); console.log('ASK_BROWSER_READY ' + JSON.stringify({ root, socket, client_socket: root + '/client.sock', subject, session_id: ready.session_id }));
      await poll(async () => !(await read()).native.pending_ask, 'browser native answer');
      const actual = JSON.parse(await readFile(root + '/native-result-' + scenario.id + '.json', 'utf8'));
      if (actual.is_error) throw Error('Browser answer did not resolve the actual native ask');
      await poll(async () => (await read()).native.idle, 'browser model continuation idle');
      console.log('ASK_BROWSER_NATIVE_RESULT ' + JSON.stringify(actual));
      break;
    }
    if (oldRef) {
      const stale = await answer('stale-' + scenarioIndex, { _tag: 'Selection', askRef: oldRef, answers: scenarios[0].answers });
      if (stale.status < 400 || stale.envelope.code !== 'ask-no-longer-pending') throw Error('Stale askRef admitted ' + JSON.stringify(stale));
      if ((await read()).native.pending_ask.tool_call_id !== scenario.id) throw Error('Stale response changed the new ask');
    }
    if (scenario.id === 'native-ask-edited') {
      await pty('send', 'native-smoke', '--seq', 'key:down');
      await poll(async () => (await read()).native.ask_reason === 'native-ask-edited-in-terminal', 'terminal edit revocation');
      const refused = await answer('edited', { _tag: 'Selection', askRef: pending.ask_ref, answers: scenario.answers });
      if (refused.status < 400 || refused.envelope.code !== 'unsupported-harness-ask') throw Error('Edited ask was not honestly refused');
      await pty('send', 'native-smoke', '--seq', 'key:return');
      await poll(async () => !(await read()).native.pending_ask, 'real terminal completion clears pending');
      proof.push({ scenario: scenario.id, refusal: refused.envelope.code });
      break;
    }
    if (!queued.native.ask_supported) throw Error('Native guarded answer unsupported ' + JSON.stringify(queued.native));
    const parameters = scenario.text === undefined ? { _tag: 'Selection', askRef: pending.ask_ref, answers: scenario.answers } : { _tag: 'Text', askRef: pending.ask_ref, text: scenario.text };
    if (scenarioIndex === 0) {
      const malformed = await answer('partial', { ...parameters, answers: scenario.answers.slice(0, 1) });
      if (malformed.status < 400 || malformed.envelope.code !== 'invalid-harness-answers') throw Error('Partial answers were admitted');
      const unauthorized = await answer('unauthorized', parameters, false);
      if (unauthorized.status !== 403) throw Error('Unpaired answer was admitted');
    }
    const accepted = await answer('valid-' + scenarioIndex, parameters);
    if (accepted.status !== 200 || accepted.value.status !== 'accepted') throw Error('Owner did not accept pending answer ' + JSON.stringify(accepted));
    const operation = accepted.value.operation_id;
    const settled = await poll(async () => { const result = await request('/v1/client/harness-control-receipts/' + encodeURIComponent(operation) + '?subject=' + encodeURIComponent(subject)); if (result.status !== 200) throw Error(JSON.stringify(result)); if (['rejected', 'indeterminate'].includes(result.value.status)) throw Error('Native answer failed ' + JSON.stringify(result.value)); return result.value.status === 'applied' ? result.value : false; }, 'actual native result ' + scenario.id);
    if (settled.result.native_event !== 'tool_result' || settled.result.tool_call_id !== scenario.id) throw Error('Receipt lacked exact native tool_result');
    await poll(async () => !(await read()).native.pending_ask, 'matching native result clears pending');
    const actual = JSON.parse(await readFile(root + '/native-result-' + scenario.id + '.json', 'utf8'));
    if (actual.is_error) throw Error('Native built-in ask errored');
    const expected = scenario.text === undefined ? scenario.answers : [{ questionId: scenario.questions[0].id, options: [], text: scenario.text }];
    const actualAnswers = scenario.questions.length === 1 ? [actual.details] : actual.details.results;
    if (actualAnswers.length !== expected.length || actualAnswers.some((answer, index) => answer.customInput !== expected[index].text || answer.selectedOptions.length !== expected[index].options.length || expected[index].options.some(option => !answer.selectedOptions.includes(option)))) throw Error('Native answers differ from the atomic submission ' + JSON.stringify(actual));
    const duplicate = await answer('duplicate-' + scenarioIndex, parameters);
    if (duplicate.status < 400 || duplicate.envelope.code !== 'ask-no-longer-pending') throw Error('Already-settled askRef was admitted');
    oldRef = pending.ask_ref;
    proof.push({ scenario: scenario.id, accepted: accepted.value.status, receipt: settled, actual });
    await poll(async () => (await read()).native.idle, 'native continuation idle');
  }
  if (!providerRequests.some(body => body.messages?.some(message => message.role === 'tool' && message.content.includes('User')))) throw Error('Actual native tool result did not reach model continuation');
  if (!(await readdir(sessions)).some(file => file.endsWith('.jsonl'))) throw Error('Native persisted transcript missing');
  await Bun.write(root + '/proof.json', JSON.stringify({ proof, providerRequests }));
  console.log('OWNER_ASK_SUCCESS ' + JSON.stringify({ root, proof, native_source: 'OMP built-in ask', provider: 'deterministic zero-cost loopback', channel: 'production omp-channel', action: 'harness.answer_ask' }));
  if (process.env.SMOKE_BROWSER_HOLD) {
    console.log('ASK_BROWSER_HOLD ' + root);
    await poll(async () => await Bun.file(root + '/stop').exists(), 'explicit browser teardown');
  }
  daemonProcess.kill(); await daemonProcess.exited;
  console.log('OWNER_ASK_DAEMON_STDERR ' + await daemonStderr);
} finally {
  clearTimeout(deadline); await pty('kill', 'native-smoke').catch(() => {}); await pty('rm', 'native-smoke').catch(() => {});
  daemonProcess?.kill(); await daemonProcess?.exited; server.stop();
  if (!process.env.SMOKE_KEEP) await rm(root, { recursive: true, force: true });
}
