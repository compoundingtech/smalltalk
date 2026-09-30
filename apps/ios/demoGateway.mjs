// A stand-in gateway with invented data, for Debug simulator screenshots without a real st or a
// real pairing. It speaks just enough of st3.client.v0 for the app: capabilities, pairing, the
// on-demand lists, a few actions, the collections socket, one conversation, and one terminal.
//
//   node demoGateway.mjs [port]      then open, in the simulator,
//   com.compoundingtech.smalltalk.starter://pair?gateway=http://<LAN or Tailscale IP>:<port>&id=demo&code=demo
//
// Never point a Release build or a real device at it: nothing here is real or authenticated.
import http from 'node:http';
// `ws` arrives with Expo's tooling; version 7's CommonJS export names the server `Server`.
import ws from 'ws';

const WebSocketServer = ws.WebSocketServer ?? ws.Server;

const port = Number(process.argv[2] ?? 8791);
const API_VERSION = 'st3.client.v0';
const now = Date.now();
const ago = (minutes) => new Date(now - minutes * 60_000).toISOString();
const snapshot = { id: 'snapshot/demo/1', host_id: 'host/harbor', store_index: 1, projection_version: 'client-projection.v0', created_at: ago(0) };
const envelope = (value) => ({ api_version: API_VERSION, request_id: `request/${Math.random().toString(16).slice(2)}`, snapshot, value });
const page = (collection, items) => ({ kind: 'page', collection, filters: {}, items, page: { has_more: false, limit: 30 } });

const agent = (name, extra = {}) => ({
  id: `agent/${name}`, kind: 'agent', name, revision: 'r1', updated_at: ago(extra.minutes ?? 30), state: 'running', harness_state: 'idle',
  driver: 'claude', host_id: 'host/harbor', reachability: 'local', runtime_ids: [`runtime/${name}`], current_session_id: `session/${name.replaceAll('/', '-')}`, ...extra,
});
const agents = [
  agent('fleet/atlas/standing/atlas', { minutes: 480 }),
  agent('fleet/beacon', { harness_state: 'working', driver: 'codex', minutes: 2, current_work: [{ id: 'step-run/r1/build', mission_id: 'mission/fleet/beacon/ship-widgets', mission_run_id: 'mission-run/r1', path: 'build', since: ago(20), state: 'claimed', title: 'Build the widget' }] }),
  agent('fleet/cedar', { minutes: 240, driver: 'omp', host_id: 'host/meadow' }),
  agent('fleet/delta/omp', { minutes: 60, driver: 'omp' }),
  agent('fleet/ember', { state: 'waiting', harness_state: 'blocked', minutes: 5 }),
  agent('fleet/fjord', { fault: 'the harness exited twice in a minute', minutes: 12 }),
  agent('fleet/grove', { minutes: 420 }),
  agent('fleet/harbor-ci', { driver: 'codex', minutes: 480, harness_state: null }),
  agent('fleet/iris', { minutes: 10 }),
  agent('fleet/juniper/builds/2026-09-30/juniper-builder', { state: 'stopped', driver: 'codex', minutes: 120, runtime_ids: [] }),
];
const sessions = [
  { id: 'session/found-1', kind: 'session', revision: 'r1', updated_at: ago(1), owner_id: 'person/demo', started_at: ago(90), state: 'running', timeline_cursor: 'c', managed: false, driver: 'codex', workspace: '/home/demo/src/garden', native_session_id: 'n1' },
  ...agents.map(item => ({ id: item.current_session_id, kind: 'session', revision: 'r1', updated_at: item.updated_at, owner_id: item.id, started_at: ago(600), state: 'running', timeline_cursor: 'c' })),
];
const attention = [
  { id: 'attention/gate', kind: 'attention', revision: 'r1', updated_at: ago(40), attention_kind: 'human-gate', title: 'Approve the widget release', detail: 'Beacon built **v2.3**. Approve to ship it to the staging fleet.', priority: 'high', state: 'open', person_id: 'person/demo', requested_at: ago(40), source_id: 'step-run/r1/approve', step_run_id: 'step-run/r1/approve', mission_id: 'mission/fleet/beacon/ship-widgets', actions: ['review.approve', 'review.reject'] },
  { id: 'attention/ask', kind: 'attention', revision: 'r1', updated_at: ago(6), attention_kind: 'agent-request', title: 'Which region should the canary use?', detail: 'I can use `eu-west` or `us-east`. Which one?', priority: 'normal', state: 'open', person_id: 'person/demo', requested_at: ago(6), source_id: 'agent/fleet/ember', requester_id: 'agent/fleet/ember', actions: ['attention.resolve'] },
  { id: 'attention/fault', kind: 'attention', revision: 'r1', updated_at: ago(12), attention_kind: 'fault', title: 'Fjord keeps crashing', detail: 'The harness exited twice in a minute.', priority: 'critical', state: 'open', person_id: 'person/demo', requested_at: ago(12), source_id: 'agent/fleet/fjord', actions: ['attention.resolve'] },
  { id: 'attention/launch', kind: 'attention', revision: 'r1', updated_at: ago(90), attention_kind: 'launch-approval', title: 'Plan: tidy the docs site', detail: 'A planner proposed a mission.', priority: 'normal', state: 'open', person_id: 'person/demo', requested_at: ago(90), source_id: 'launch/docs', actions: ['launch.approve', 'launch.cancel'] },
  { id: 'attention/msg1', kind: 'attention', revision: 'r1', updated_at: ago(180), attention_kind: 'unread-message', title: 'Nightly report is ready', detail: 'Unread message from agent/fleet/atlas/standing/atlas.', priority: 'low', state: 'open', person_id: 'person/demo', requested_at: ago(180), source_id: 'message/m1', message_id: 'message/m1', actions: ['message.read'] },
  { id: 'attention/msg2', kind: 'attention', revision: 'r1', updated_at: ago(300), attention_kind: 'unread-message', title: 'Re: cache warmup numbers', detail: 'Unread message from agent/fleet/iris.', priority: 'low', state: 'open', person_id: 'person/demo', requested_at: ago(300), source_id: 'message/m2', message_id: 'message/m2', actions: ['message.read'] },
];
const step = (path, state, extra = {}) => ({ id: `step-run/r1/${path}`, path, state, attempt: 1, since: ago(30), goals: [`Do the ${path} step.`], ...extra });
const mission = (id, state, steps, minutes = 60) => ({ id: `mission/${id}`, kind: 'mission', title: id, revision: 'r1', updated_at: ago(minutes), mission_revision: 'm1', run_generations: {}, runs: ['mission-run/r1'], state, run_details: [{ id: 'mission-run/r1', status: state, steps, current_steps: [] }] });
const missions = [
  mission('fleet/beacon/ship-widgets', 'running', [step('design', 'completed'), step('build', 'claimed', { claimant: 'agent/fleet/beacon' }), step('approve', 'waiting'), step('deploy', 'pending')], 20),
  mission('fleet/cedar/garden-sync', 'running', [step('sync', 'ready')], 50),
  mission('fleet/grove/nightly-report', 'standing', [step('keep-watch', 'running', { agentless: true })], 400),
  mission('fleet/iris/cache-warmup', 'completed', [step('warm', 'completed'), step('measure', 'completed')], 700),
  mission('fleet/fjord/index-rebuild', 'running', [step('rebuild', 'blocked', { blocked_reason: 'waiting on fjord to restart' })], 15),
  mission('fleet/harbor/ci/run', 'running', [step('test', 'running', { agentless: true })], 3),
];
const machines = [
  { id: 'machine/harbor', kind: 'machine', revision: 'r1', updated_at: ago(0), name: 'harbor', host_id: 'host/harbor', state: 'local', capacity: { state: 'reported', reason: '' }, occupancy: { running_runtimes: 9 }, projects: [], runtime_ids: [], transports: [{ protocol: 'unix', status: 'local' }], work: [] },
  { id: 'machine/meadow', kind: 'machine', revision: 'r1', updated_at: ago(1), name: 'meadow', host_id: 'host/meadow', state: 'reachable', capacity: { state: 'reported', reason: '' }, occupancy: { running_runtimes: 2 }, projects: [], runtime_ids: [], transports: [{ protocol: 'fabric', status: 'up' }], work: [] },
];
const devices = [{ id: 'device/demo-phone', kind: 'device', revision: 'r1', updated_at: ago(5), name: 'Demo phone', session_actor: 'person/demo', person_id: 'person/demo', state: 'active', expires_at: '2026-12-31T00:00:00Z', scopes: ['read.projections', 'attention', 'message.send', 'terminal.input'] }];
const launches = [{ id: 'launch/docs', kind: 'launch', revision: 'r1', updated_at: ago(90), title: 'Tidy the docs site', request: 'Find broken links and **fix** them.', phase: 'review', planner_config: { provider: 'claude' } }];

let sequence = 0;
const entry = (minutes, type, role, body) => ({ id: `timeline-entry/demo/${++sequence}`, sequence, revision: 1, final: true, timestamp: ago(minutes), type, role, body });
const conversation = () => {
  sequence = 0;
  return [
    entry(50, 'content', 'user', { text: '<system-reminder>context for the model</system-reminder>Can you look at why the widget build is slow?' }),
    entry(49, 'content', 'assistant', { text: 'Sure. I will time each **stage** first.\n\n- compile\n- link\n- package' }),
    entry(48, 'tool_call', 'assistant', { call_id: 'c1', name: 'Bash', arguments: { command: 'make -j8 widget 2>&1 | tail -20' } }),
    entry(47, 'tool_result', 'tool', { call_id: 'c1', status: 'success', content: Array.from({ length: 12 }, (_, n) => `[${n + 1}/12] compiled module_${n}.o`).join('\n') }),
    entry(46, 'status', 'system', { status: 'running' }),
    entry(45, 'content', 'assistant', { text: 'Linking takes 80% of the time. The `lto` flag is on in debug builds; turning it off there cuts the build from **4m** to **50s**.' }),
    entry(40, 'error', 'system', { code: 'native-delivery-degraded', message: 'Native conversation delivery over claude-channel paused while the st daemon was unreachable; the driver stayed online and retried every second.', retryable: true, details: { severity: 'warning' } }),
    entry(40, 'error', 'system', { code: 'native-delivery-recovered', message: 'Native conversation delivery over claude-channel recovered and resumed replay from durable graph state.', retryable: true, details: { severity: 'warning' } }),
    entry(30, 'error', 'system', { code: 'native-delivery-degraded', message: 'Native conversation delivery over claude-channel paused while the st daemon was unreachable; the driver stayed online and retried every second.', retryable: true, details: { severity: 'warning' } }),
    entry(30, 'error', 'system', { code: 'native-delivery-recovered', message: 'Native conversation delivery over claude-channel recovered and resumed replay from durable graph state.', retryable: true, details: { severity: 'warning' } }),
    entry(20, 'message', 'user', { message_id: 'message/m9', from: 'agent/fleet/atlas/standing/atlas', to: 'agent/fleet/beacon', title: 'Ship it after the canary' }),
    entry(20, 'content', 'user', { text: 'Once the canary is green for an hour, go ahead. [id:message/m9]' }),
    entry(18, 'tool_call', 'assistant', { call_id: 'c2', name: 'Edit', arguments: { file_path: 'build/profiles.toml' } }),
    entry(18, 'tool_result', 'tool', { call_id: 'c2', status: 'success', content: '-lto = true\n+lto = false' }),
    entry(10, 'message', 'user', { message_id: 'message/m10', from: 'person/demo', to: 'agent/fleet/beacon', title: 'Nice' }),
    entry(10, 'content', 'user', { text: 'Nice work. Open a PR when it is ready.' }),
    entry(9, 'content', 'assistant', { text: 'Will do. Running the full test suite now.' }),
    entry(8, 'tool_call', 'assistant', { call_id: 'c3', name: 'Bash', arguments: { command: 'cargo test --workspace' } }),
  ];
};
const screen = {
  kind: 'terminal-screen', terminal_id: 'terminal/demo', runtime_incarnation: 'inc-1', revision: 's1', next_sequence: 1, columns: 60, rows: 8, title: 'beacon', truncated: false,
  cursor: { row: 7, column: 2, visible: true, blinking: false, style: 'block' }, modes: {},
  lines: [
    { row: 0, text: '', runs: [{ text: '╭─ beacon ', fg: 4, bold: true }, { text: 'working', fg: 3 }], redacted: false, truncated: false },
    { row: 1, text: '│ Running the full test suite now.', runs: [], redacted: false, truncated: false },
    { row: 2, text: '', runs: [{ text: '│ ', fg: 8 }, { text: '$ cargo test --workspace', fg: 2 }], redacted: false, truncated: false },
    { row: 3, text: '│ test result: ok. 214 passed; 0 failed', runs: [], redacted: false, truncated: false },
    { row: 4, text: '╰─', runs: [], redacted: false, truncated: false },
    { row: 5, text: '', runs: [], redacted: false, truncated: false },
    { row: 6, text: '', runs: [{ text: '> ', fg: 5, bold: true }], redacted: false, truncated: false },
  ],
};

const capabilities = {
  kind: 'capabilities', session_actor: 'person/demo', transport: 'fabric-loopback', event_cursor: 'e', oldest_event_cursor: 'e', schemas: [],
  limits: { max_page_items: 30, max_event_items: 100, max_response_bytes: 1_000_000, max_wait_ms: 30_000 },
  capabilities: [{ id: 'terminal.input', state: 'granted', version: 1 }, { id: 'message.send', state: 'granted', version: 1 }],
};

const server = http.createServer((request, response) => {
  const url = new URL(request.url, 'http://demo');
  const send = (value, status = 200) => { response.writeHead(status, { 'content-type': 'application/json' }); response.end(JSON.stringify(envelope(value))); };
  let body = '';
  request.on('data', chunk => { body += chunk; });
  request.on('end', () => {
    const path = url.pathname;
    console.log(request.method, path);
    if (path === '/v1/client/capabilities') return send(capabilities);
    if (/^\/v1\/client\/pairings\/[^/]+\/complete$/.test(path)) return send({ kind: 'paired-session', credential: 'demo-credential', device_id: 'device/demo-phone', expires_at: '2026-12-31T00:00:00Z', person_id: 'person/demo', scopes: [], session_actor: 'person/demo' });
    if (path === '/v1/client/sessions') return send(page('sessions', url.searchParams.get('history') === 'true' ? [] : sessions));
    if (path === '/v1/client/machines') return send(page('machines', machines));
    if (path === '/v1/client/devices') return send(page('devices', devices));
    if (path === '/v1/client/launches') return send(page('launches', launches));
    if (/^\/v1\/client\/launches\/[^/]+\/variants$/.test(path)) return send(page('launch-variants', [{ id: 'launch-variant/docs-1', kind: 'launch-variant', revision: 'r1', updated_at: ago(80), ordinal: 1, status: 'ready', diagnostics: [] }]));
    const missionMatch = /^\/v1\/client\/missions\/(.+)$/.exec(path);
    if (missionMatch) { const found = missions.find(item => item.id === decodeURIComponent(missionMatch[1])); return found ? send(found) : send({ error_version: 1, code: 'not-found', message: 'no such mission' }, 404); }
    const runtimeMatch = /^\/v1\/client\/runtimes\/(.+)$/.exec(path);
    if (runtimeMatch) return send({ kind: 'runtime', id: decodeURIComponent(runtimeMatch[1]), revision: 'r1', updated_at: ago(1), desired_revision: null, incarnation_id: 'inc-1', owner_host_id: 'host/harbor', owner_id: 'agent/fleet/beacon', runtime_id: 'demo', runtime_kind: 'agent', state: 'running', terminal_id: 'terminal/demo' });
    if (/^\/v1\/client\/terminals\/[^/]+\/screen$/.test(path)) return send(screen);
    if (path === '/v1/client/actions') {
      let action = {};
      try { action = JSON.parse(body); } catch { /* keep empty */ }
      return send({ kind: 'action-result', action_id: action.id ?? 'action/demo', affected_ids: [], operation_id: 'operation/demo', snapshot_id: snapshot.id, status: 'completed', ...(action.type === 'terminal.attach' ? { terminal_attachment: { stream_capability: 'demo', runtime_incarnation: 'inc-1' } } : {}) });
    }
    send({ error_version: 1, code: 'not-found', message: `the demo gateway has no ${path}` }, 404);
  });
});

const collections = new WebSocketServer({ noServer: true, handleProtocols: protocols => [...protocols][0] });
server.on('upgrade', (request, socket, head) => collections.handleUpgrade(request, socket, head, ws => collections.emit('connection', ws, request)));
const windows = { attention, missions, agents };
collections.on('connection', ws => {
  ws.on('message', data => {
    const command = JSON.parse(String(data));
    if (command.kind !== 'subscribe') return;
    if (command.collection in windows) {
      const items = windows[command.collection];
      ws.send(JSON.stringify({ kind: 'snapshot', id: command.id, collection: command.collection, snapshot, items, order: items.map(item => item.id), has_more: false }));
    } else if (command.collection === 'conversation') {
      ws.send(JSON.stringify({ kind: 'conversation', id: command.id, collection: 'conversation', session_id: 'session/demo', replace: true, items: conversation(), has_more: true }));
    } else if (command.collection === 'terminal') {
      ws.send(JSON.stringify({ kind: 'screen', id: command.id, collection: 'terminal', snapshot, value: screen }));
    }
  });
});

server.listen(port, '0.0.0.0', () => console.log(`demo gateway on :${port}`));
