import assert from 'node:assert/strict';
import { agentName, agentRows, agentSections, agentState, agentTreeLines, filterAgentRows, UNMANAGED_GROUP } from './agentsView.ts';

const now = Date.parse('2026-09-30T12:00:00Z');
const agent = (id, extra = {}) => ({ id: `agent/${id}`, name: id, kind: 'agent', updated_at: '2026-09-30T11:00:00Z', state: 'running', harness_state: 'idle', driver: 'claude', runtime_ids: [], reachability: 'local', ...extra });

// Names read the way stui reads them, including an omp seat under its parent.
assert.equal(agentName({ id: 'agent/fleet/cos/standing/cos', name: 'fleet/cos/standing/cos' }), 'COS');
assert.equal(agentName({ id: 'agent/fleet/pty-rust/omp', name: 'fleet/pty-rust/omp' }), 'PTY Rust · OMP');
assert.equal(agentName({ id: 'agent/fleet/smalltalk-ci', name: 'fleet/smalltalk-ci' }), 'Smalltalk Ci');

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
  agent('fleet/zeta'),
  agent('fleet/alpha'),
  agent('fleet/busy', { harness_state: 'working', driver: 'codex' }),
  agent('fleet/asks', { state: 'waiting', harness_state: 'blocked' }),
  agent('fleet/off', { state: 'stopped' }),
  agent('fleet/bad', { fault: 'crashed' }),
];
const sessions = [
  { id: 'session/found', kind: 'session', state: 'running', managed: false, driver: 'codex', workspace: '/home/me/src/app', updated_at: '2026-09-30T11:59:18Z' },
  { id: 'session/managed', kind: 'session', state: 'running', owner_id: 'agent/fleet/alpha', updated_at: '2026-09-30T11:00:00Z' },
  { id: 'session/old', kind: 'session', state: 'completed', managed: false, driver: 'codex', updated_at: '2026-09-30T11:00:00Z' },
];
const rows = agentRows(agents, sessions, 'hetz', now);
assert.deepEqual(rows.map(row => row.name), ['Asks', 'Bad', 'Busy', 'Alpha', 'Zeta', 'Off', 'codex in app']);
assert.deepEqual(agentSections(rows).map(section => [section.title, section.count]), [['waiting on you', 1], ['broken', 1], ['working', 1], ['idle', 2], ['stopped', 1], [UNMANAGED_GROUP, 1]]);
assert.equal(agentSections(rows)[0].person, true);
const found = rows.at(-1);
assert.deepEqual([found.target, found.harness, found.activity, found.host, found.unmanaged], ['session/found', 'codex', '42s', 'hetz', true]);
assert.deepEqual([rows[3].path, rows[3].harness, rows[3].activity, rows[3].target], ['fleet/alpha', 'claude', '1h', 'agent/fleet/alpha']);

// The tree puts each agent under the folders of its path, opening each folder once.
const tree = agentTreeLines(agentRows([agent('fleet/cos/standing/cos'), agent('fleet/stui'), agent('fleet/cos/other')], [], 'hetz', now));
assert.deepEqual(tree.map(line => line.kind === 'folder' ? `${'  '.repeat(line.depth)}${line.name}/` : `${'  '.repeat(line.depth)}${line.row.name}`), [
  'fleet/',
  '  cos/',
  '    Other',
  '    standing/',
  '      COS',
  '  Stui',
]);

// The search field keeps rows matching every word, in name, path, harness, or host.
assert.deepEqual(filterAgentRows(rows, 'CODEX').map(row => row.name), ['Busy', 'codex in app']);
assert.deepEqual(filterAgentRows(rows, 'fleet ZE').map(row => row.name), ['Zeta']);
assert.equal(filterAgentRows(rows, '  ').length, rows.length);
