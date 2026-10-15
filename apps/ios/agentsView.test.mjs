import assert from 'node:assert/strict';
import { agentModel, compactFolders, agentName, agentRows, agentSections, agentState, agentTreeLines, agentWord, filterAgentRows, stepProgress, UNMANAGED_GROUP } from './agentsView.ts';

const now = Date.parse('2026-09-30T12:00:00Z');
const served = (state) => ({ presentation: { state, group: state, because: null, now: null, next: [], queued: 0 } });
const agent = (id, extra = {}, state = 'idle') => ({ id: `agent/${id}`, name: id, kind: 'agent', updated_at: '2026-09-30T11:00:00Z', state: 'running', harness_state: 'idle', driver: 'claude', runtime_ids: [], reachability: 'local', ...served(state), ...extra });

// Names read the way stui reads them, including an omp seat under its parent.
assert.equal(agentName({ id: 'agent/example/cos/standing/cos', name: 'example/cos/standing/cos' }), 'COS');
assert.equal(agentName({ id: 'agent/example/pty-rust/omp', name: 'example/pty-rust/omp' }), 'PTY Rust · OMP');
// A seat named after its host (st agents new NAME) is labelled without the host.
assert.equal(agentName({ id: 'agent/harbor.image-sorter', name: 'harbor.image-sorter', host_id: 'host/harbor' }), 'Image Sorter');
assert.equal(agentName({ id: 'agent/v2.parser', name: 'v2.parser', host_id: 'host/harbor' }), 'V2.parser');
assert.equal(agentName({ id: 'agent/example/smalltalk-ci', name: 'example/smalltalk-ci' }), 'Smalltalk Ci');

// st serves each agent's state; the phone reads it and derives nothing.
assert.equal(agentState(served('working')), 'working');
assert.equal(agentState(served('needs_you')), 'needs-you');
assert.equal(agentState(served('needs_login')), 'needs-login');
assert.equal(agentState(served('down')), 'down');
assert.equal(agentState({ state: 'running', harness_state: 'working' }), 'unknown', 'an older daemon serves no presentation');
// An older st's row shows the state it reports, marked so, and derives nothing.
import { agentReported } from './agentsView.ts';
assert.equal(agentReported({ state: 'running' }), 'running · older st');
assert.equal(agentReported(served('working')), undefined);
assert.equal(agentRows([{ ...agent('example/old'), presentation: undefined }], [], 'example-linux', now)[0].reported, 'running · older st');
assert.equal(agentState(served('something-new')), 'unknown');
assert.equal(agentWord('compacting'), 'compacting');
assert.equal(agentWord('needs-you'), 'alert');
// A retired seat is retired as st serves it, in its own section.
assert.equal(agentState(served('retired')), 'retired');
assert.equal(agentWord('retired'), 'retired');
assert.deepEqual(agentSections(agentRows([agent('example/retired', {}, 'retired')], [], 'example-linux', now)).map(section => section.title), ['retired']);

// Grouped and sorted like stui: waiting on you, broken, working, idle, stopped, and sessions st
// found but did not start last, never first.
const agents = [
  agent('example/zeta'),
  agent('example/alpha'),
  agent('example/busy', { harness_state: 'working', driver: 'codex' }, 'working'),
  agent('example/asks', { state: 'waiting', harness_state: 'blocked' }, 'needs_you'),
  agent('example/off', { state: 'stopped' }, 'stopped'),
  agent('example/bad', { fault: 'crashed' }, 'fault'),
];
const sessions = [
  { id: 'session/found', kind: 'session', state: 'running', managed: false, driver: 'codex', workspace: '/home/example/src/app', updated_at: '2026-09-30T11:59:18Z' },
  { id: 'session/managed', kind: 'session', state: 'running', owner_id: 'agent/example/alpha', updated_at: '2026-09-30T11:00:00Z' },
  { id: 'session/old', kind: 'session', state: 'completed', managed: false, driver: 'codex', updated_at: '2026-09-30T11:00:00Z' },
];
const rows = agentRows(agents, sessions, 'example-linux', now);
assert.deepEqual(rows.map(row => row.name), ['Asks', 'Bad', 'Busy', 'Alpha', 'Zeta', 'Off', 'codex in app']);
assert.deepEqual(agentSections(rows).map(section => [section.title, section.count]), [['alerts', 1], ['broken', 1], ['working', 1], ['idle', 2], ['stopped', 1], [UNMANAGED_GROUP, 1]]);
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

// Signed out of its provider: the phone says how to log in.
import { agentGroup, loginGuidance } from './agentsView.ts';
assert.match(loginGuidance({ driver: 'claude', host_id: 'host/harbor' }), /Claude login required on harbor: open its terminal and run \/login/);

assert.equal(agentWord('down'), 'down');
assert.equal(agentGroup({ state: 'down', unmanaged: false }), 'broken');

// A seat's status is what the step it holds last reported, on one line (Nathan, 2026-10-06).
const missions = [{ id: 'mission/example/build', run_details: [{ steps: [
  { id: 'step-run/example/one', last_progress: '  Tests pass on\nmain; opening the PR \n' },
  { id: 'step-run/example/quiet', last_progress: null },
] }] }];
assert.equal(stepProgress(missions, 'step-run/example/one'), 'Tests pass on main; opening the PR');
assert.equal(stepProgress(missions, 'step-run/example/quiet'), undefined);
assert.equal(stepProgress(missions, 'step-run/example/unknown'), undefined);
const working = { ...agents[0], id: 'agent/example/doer', current_work: [{ id: 'step-run/example/one', mission_id: 'mission/example/build', path: 'build' }] };
assert.equal(agentRows([working], [], 'example-linux', now, missions)[0].progress, 'Tests pass on main; opening the PR');
assert.equal(agentRows([working], [], 'example-linux', now)[0].progress, undefined);

// A compacting seat is listed with the working ones, and says so.
const compacting = agentRows([agent('example/tidy', { harness_state: 'working', activity: 'compacting' }, 'compacting')], [], 'example-linux', now);
assert.equal(compacting[0].state, 'compacting');
assert.deepEqual(agentSections(compacting).map(section => [section.title, section.count]), [['working', 1]]);
assert.equal(agentSections(compacting)[0].person, false);
