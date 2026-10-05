import assert from 'node:assert/strict';
import { agentModel, compactFolders, agentName, agentRows, agentSections, agentState, agentTreeLines, filterAgentRows, UNMANAGED_GROUP } from './agentsView.ts';

const now = Date.parse('2026-09-30T12:00:00Z');
const agent = (id, extra = {}) => ({ id: `agent/${id}`, name: id, kind: 'agent', updated_at: '2026-09-30T11:00:00Z', state: 'running', harness_state: 'idle', driver: 'claude', runtime_ids: [], reachability: 'local', ...extra });

// Names read the way stui reads them, including an omp seat under its parent.
assert.equal(agentName({ id: 'agent/example/cos/standing/cos', name: 'example/cos/standing/cos' }), 'COS');
assert.equal(agentName({ id: 'agent/example/pty-rust/omp', name: 'example/pty-rust/omp' }), 'PTY Rust · OMP');
// A seat named after its host (st agents new NAME) is labelled without the host.
assert.equal(agentName({ id: 'agent/harbor.image-sorter', name: 'harbor.image-sorter', host_id: 'host/harbor' }), 'Image Sorter');
assert.equal(agentName({ id: 'agent/v2.parser', name: 'v2.parser', host_id: 'host/harbor' }), 'V2.parser');
assert.equal(agentName({ id: 'agent/example/smalltalk-ci', name: 'example/smalltalk-ci' }), 'Smalltalk Ci');

// States follow stui: a fault or a stale delivery is broken whatever the harness says.
assert.equal(agentState({ state: 'running', harness_state: 'working' }), 'working');
assert.equal(agentState({ state: 'running', harness_state: 'idle' }), 'idle');
assert.equal(agentState({ state: 'running', harness_state: 'working', fault: 'boom' }), 'fault');
assert.equal(agentState({ state: 'running', delivery: { state: 'stale' } }), 'fault');
assert.equal(agentState({ state: 'waiting', harness_state: 'blocked' }), 'needs-you');
assert.equal(agentState({ state: 'waiting', harness_state: 'starting' }), 'starting');
assert.equal(agentState({ state: 'stopped' }), 'stopped');
assert.equal(agentState({ state: 'failed' }), 'fault');

// Grouped and sorted like stui: waiting on you, broken, working, idle, stopped, and sessions st
// found but did not start last, never first.
const agents = [
  agent('example/zeta'),
  agent('example/alpha'),
  agent('example/busy', { harness_state: 'working', driver: 'codex' }),
  agent('example/asks', { state: 'waiting', harness_state: 'blocked' }),
  agent('example/off', { state: 'stopped' }),
  agent('example/bad', { fault: 'crashed' }),
];
const sessions = [
  { id: 'session/found', kind: 'session', state: 'running', managed: false, driver: 'codex', workspace: '/home/example/src/app', updated_at: '2026-09-30T11:59:18Z' },
  { id: 'session/managed', kind: 'session', state: 'running', owner_id: 'agent/example/alpha', updated_at: '2026-09-30T11:00:00Z' },
  { id: 'session/old', kind: 'session', state: 'completed', managed: false, driver: 'codex', updated_at: '2026-09-30T11:00:00Z' },
];
const rows = agentRows(agents, sessions, 'example-linux', now);
assert.deepEqual(rows.map(row => row.name), ['Asks', 'Bad', 'Busy', 'Alpha', 'Zeta', 'Off', 'codex in app']);
assert.deepEqual(agentSections(rows).map(section => [section.title, section.count]), [['waiting on you', 1], ['broken', 1], ['working', 1], ['idle', 2], ['stopped', 1], [UNMANAGED_GROUP, 1]]);
assert.equal(agentSections(rows)[0].person, true);
const found = rows.at(-1);
assert.deepEqual([found.target, found.harness, found.activity, found.host, found.unmanaged], ['session/found', 'codex', '42s', 'example-linux', true]);
assert.deepEqual([rows[3].path, rows[3].harness, rows[3].activity, rows[3].target], ['example/alpha', 'claude', '1h', 'agent/example/alpha']);

// The tree puts each agent under the folders of its path, opening each folder once.
const tree = agentTreeLines(agentRows([agent('example/cos/standing/cos'), agent('example/stui'), agent('example/cos/other')], [], 'example-linux', now));
assert.deepEqual(tree.map(line => line.kind === 'folder' ? `${'  '.repeat(line.depth)}${line.name}/` : `${'  '.repeat(line.depth)}${line.row.name}`), [
  'example/',
  '  cos/',
  '    Other',
  '    standing/',
  '      COS',
  '  Stui',
]);

// The search field keeps rows matching every word, in name, path, harness, or host.
assert.deepEqual(filterAgentRows(rows, 'CODEX').map(row => row.name), ['Busy', 'codex in app']);
assert.deepEqual(filterAgentRows(rows, 'example ZE').map(row => row.name), ['Zeta']);
assert.equal(filterAgentRows(rows, '  ').length, rows.length);

// A folder holding only one folder joins it on one line.
assert.deepEqual(compactFolders([
  'fleet/smalltalk/operations/2026-10-01/operator'.split('/'),
  'fleet/smalltalk/ci/watcher'.split('/'),
  'fleet/cos/standing/cos'.split('/'),
  ['solo'],
]), [['fleet', 'smalltalk', 'operations/2026-10-01'], ['fleet', 'smalltalk', 'ci'], ['fleet', 'cos/standing'], []]);

// The model beside the harness is what the harness last reported, and nothing when st says none.
assert.equal(agentModel({ usage: { context: { model: 'claude-sonnet-5-5' } } }), 'claude-sonnet-5-5');
assert.equal(agentModel({ usage: { context: { model: ' ' } } }), null);
assert.equal(agentModel({ usage: { context: null } }), null);
assert.equal(agentModel({ usage: null }), null);
assert.equal(agentModel({}), null);

// Signed out of its provider: needs login, said how, and it clears once signed in again.
import { agentState as stateOf, loginGuidance } from './agentsView.ts';
assert.equal(stateOf({ state: 'waiting', harness_state: 'unauthenticated', fault: null, delivery: null }), 'needs-login');
assert.equal(stateOf({ state: 'waiting', harness_state: 'idle', reason: 'providerAuth', fault: null, delivery: null }), 'needs-login');
assert.equal(stateOf({ state: 'running', harness_state: 'idle', fault: null, delivery: null }), 'idle');
assert.match(loginGuidance({ driver: 'claude', host_id: 'host/harbor' }), /Claude login required on harbor: open its terminal and run \/login/);

// An idle seat st has not heard from lately reads idle, not starting.
assert.equal(stateOf({ state: 'waiting', harness_state: 'indeterminate', observation: 'stale', reachability: 'reachable', fault: null, delivery: null }), 'idle');
assert.equal(stateOf({ state: 'waiting', harness_state: 'indeterminate', observation: 'current', reachability: 'reachable', fault: null, delivery: null }), 'starting');
assert.equal(stateOf({ state: 'waiting', harness_state: 'indeterminate', observation: null, reachability: 'reachable', fault: null, delivery: null }), 'starting');
assert.equal(stateOf({ state: 'waiting', harness_state: 'indeterminate', observation: 'stale', reachability: 'unreachable', fault: null, delivery: null }), 'starting');

// st's additive harness_error_state names a login outright, even when the harness state is stale.
assert.equal(stateOf({ state: 'waiting', harness_state: 'indeterminate', observation: 'stale', reachability: 'reachable', harness_error_state: 'needs-login', fault: null, delivery: null }), 'needs-login');
assert.equal(stateOf({ state: 'waiting', harness_state: 'indeterminate', observation: 'stale', reachability: 'reachable', harness_error_state: null, fault: null, delivery: null }), 'idle');
