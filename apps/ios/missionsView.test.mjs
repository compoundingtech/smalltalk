import assert from 'node:assert/strict';
import { isSystemMission, missionRows, missionSections, missionTitle, missionWord } from './missionsView.ts';

const step = (id, state, extra = {}) => ({ id: `step-run/${id}`, path: id, state, attempt: 1, since: '2026-09-30T11:00:00Z', ...extra });
const mission = (id, state, steps = [], extra = {}) => ({ id: `mission/${id}`, kind: 'mission', title: id, state, runs: ['r'], run_details: [{ id: 'mission-run/r', status: state, steps }], updated_at: '2026-09-30T11:00:00Z', ...extra });

assert.equal(missionWord(mission('a', 'running', [step('s', 'ready')]), [{ attention_kind: 'human-gate', mission_id: 'mission/a', state: 'open' }], []), 'decision');
assert.equal(missionWord(mission('a', 'running', [step('s', 'blocked')]), [], []), 'stalled');
assert.equal(missionWord(mission('a', 'running', [step('s', 'running')]), [], []), 'working');
assert.equal(missionWord(mission('a', 'standing', [step('keep-watch', 'running', { agentless: true }), step('x', 'completed')]), [], []), 'watching');
assert.equal(missionWord(mission('a', 'running', [step('s', 'ready')]), [], []), 'unclaimed');
assert.equal(missionWord(mission('a', 'running', [step('s', 'ready')]), [], [{ state: 'stopped', next_work_id: 'step-run/s' }]), 'unstaffed');
assert.equal(missionWord(mission('a', 'running', [step('s', 'ready')]), [], [{ state: 'running', next_work_id: 'step-run/s' }]), 'queued');
assert.equal(missionWord(mission('a', 'completed', [step('s', 'failed')]), [], []), 'done');
assert.equal(missionWord(mission('a', 'standing', []), [], []), 'idle');

assert.equal(missionTitle({ id: 'mission/fleet/stui/client-api', title: 'fleet/stui/client-api' }), 'fleet/stui · Client API');
assert.equal(isSystemMission('mission/fleet/smalltalk/ci/run'), true);
assert.equal(isSystemMission('mission/__st3/loop'), true);
assert.equal(isSystemMission('mission/fleet/stui/rebuild'), false);

const missions = [mission('fleet/b/idle', 'standing'), mission('fleet/a/work', 'running', [step('x', 'running'), step('y', 'completed')]), mission('fleet/smalltalk/ci/run', 'running', [step('z', 'running')])];
const { rows, hidden } = missionRows(missions, [], [], false, Date.parse('2026-09-30T12:00:00Z'));
assert.deepEqual(rows.map(row => [row.word, row.title, row.done, row.total]), [['working', 'fleet/a · Work', 1, 2], ['idle', 'fleet/b · Idle', 0, 0]]);
assert.equal(hidden, 1);
assert.deepEqual(missionSections(rows).map(section => section.title), ['working', 'idle']);
assert.equal(missionRows(missions, [], [], true).rows.length, 3);
