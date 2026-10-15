import assert from 'node:assert/strict';
import { actorName, filterMissionRows, missionLabel, missionRows, missionSections, missionTitle, missionWord, stepStyle, wordName } from './missionsView.ts';

const step = (id, state, extra = {}) => ({ id: `step-run/${id}`, path: id, state, attempt: 1, since: '2026-09-30T11:00:00Z', ...extra });
const mission = (id, state, steps = [], extra = {}) => ({ id: `mission/${id}`, kind: 'mission', title: id, state, runs: ['r'], run_details: [{ id: 'mission-run/r', status: state, steps }], updated_at: '2026-09-30T11:00:00Z', ...extra });
// A mission row as st serves it, with its presentation (docs/clients/ui-contract.md, Mission).
const served = (row, word, presentation = {}) => ({
  ...row,
  presentation: {
    word, system: row.system === true, who: { kind: 'nobody' }, steps: [], stuck: null, queued_behind: null, card: null, actions: [],
    progress: { done: 0, total: 0 }, last_progress: null, settled_at: null, ...presentation,
  },
});

// The phone reads the word st serves and derives none.
for (const word of ['decision', 'stalled', 'unstaffed', 'unclaimed', 'queued', 'working', 'watching', 'held', 'idle', 'done', 'failed', 'cancelled', 'not_started']) {
  assert.equal(missionWord(served(mission('a', 'running'), word)), word);
}
// The word st serves wins over anything the row's steps suggest.
assert.equal(missionWord(served(mission('a', 'running', [step('s', 'blocked')]), 'queued')), 'queued');
// An older st serves none, and a word this phone does not know is not guessed at.
assert.equal(missionWord(mission('a', 'running', [step('s', 'running')])), 'unknown');
assert.equal(missionWord(served(mission('a', 'running'), 'becalmed')), 'unknown');
assert.equal(missionLabel(mission('a', 'running')), 'running · st does not say');
assert.equal(missionLabel(served(mission('a', 'running'), 'decision')), 'needs you');
assert.equal(wordName('not_started'), 'not started');

for (const [word, name] of Object.entries({ needs_you: 'needs you', done: 'done', working: 'working', ready: 'ready', waiting: 'waiting', cancelled: 'cancelled', failed: 'failed', pending: 'later' })) {
  assert.equal(stepStyle(word).word, name, word);
}
assert.equal(stepStyle('working', '*').glyph, '*');
assert.equal(stepStyle(undefined).word, 'unknown', 'an older st serves no step word');
assert.equal(stepStyle('claimed').word, 'unknown', 'a raw step state is not a served word');

assert.equal(actorName({ kind: 'person', person: 'person/avery', label: 'Avery' }, 'person/avery'), 'you');
assert.equal(actorName({ kind: 'person', person: 'person/avery', label: 'Avery' }, 'person/blair'), 'Avery');
assert.equal(actorName({ kind: 'agent', agent: 'agent/keeper', label: 'keeper' }), 'keeper');
assert.equal(actorName({ kind: 'st' }), 'st');
assert.equal(actorName({ kind: 'nobody' }), undefined);

assert.equal(missionTitle({ id: 'mission/fleet/stui/client-api', title: 'fleet/stui/client-api' }), 'fleet/stui · Client API');

const missions = [
  served(mission('fleet/b/idle', 'standing'), 'idle'),
  served(mission('fleet/a/work', 'running', [step('x', 'running'), step('y', 'completed')]), 'working', { progress: { done: 1, total: 2 } }),
  served(mission('fleet/smalltalk/ci/run', 'running', [step('z', 'running')], { system: true }), 'working'),
];
const { rows, hidden } = missionRows(missions, false, Date.parse('2026-09-30T12:00:00Z'));
assert.deepEqual(rows.map(row => [row.word, row.title, row.done, row.total]), [['working', 'fleet/a · Work', 1, 2], ['idle', 'fleet/b · Idle', 0, 0]]);
assert.equal(hidden, 1);
assert.deepEqual(missionSections(rows).map(section => section.title), ['working', 'idle']);
assert.equal(missionRows(missions, true).rows.length, 3);
// Rows from an older st list last, under "st does not say", with their raw state.
{
  const { rows } = missionRows([mission('fleet/c/old', 'running'), ...missions], true);
  assert.equal(rows.at(-1).label, 'running · st does not say');
  assert.equal(missionSections(rows).at(-1).title, 'st does not say');
}

// A failed mission ages out of the list after the day it settled, and showing hidden ones brings it back.
{
  const now = new Date(2026, 9, 2, 22, 0).getTime();
  const failedAt = (at) => served({ id: `mission/example/failed-${at}`, title: 'failed', state: 'failed', updated_at: new Date(2026, 9, 2, 21, 0).toISOString(), run_details: null }, 'failed', { settled_at: new Date(at).toISOString() });
  const yesterday = failedAt(new Date(2026, 9, 1, 23, 0).getTime());
  const today = failedAt(new Date(2026, 9, 2, 9, 0).getTime());
  const { rows, hidden } = missionRows([yesterday, today], false, now);
  assert.deepEqual(rows.map(row => row.mission.id), [today.id]);
  assert.equal(hidden, 1);
  assert.equal(missionRows([yesterday, today], true, now).rows.length, 2);
}

// A run in progress shows what its step last reported (Nathan, 2026-10-06), as st serves it.
{
  const busy = served(mission('busy', 'running'), 'working', { last_progress: ' Tests pass;\n opening the PR ' });
  const quiet = served(mission('quiet', 'running'), 'working');
  const { rows } = missionRows([busy, quiet], true, Date.parse('2026-09-30T12:00:00Z'));
  const byId = Object.fromEntries(rows.map(row => [row.mission.id, row.progress]));
  assert.deepEqual(byId, { 'mission/busy': 'Tests pass; opening the PR', 'mission/quiet': undefined });
}

// Resources keeps mission search usable across names, paths, status and current progress.
const searchable = missionRows([
  served(mission('demo/ship', 'running'), 'working', { last_progress: 'Checking package' }),
  served(mission('demo/idle', 'standing'), 'idle'),
], true).rows;
assert.deepEqual(filterMissionRows(searchable, 'DEMO checking').map(row => row.mission.id), ['mission/demo/ship']);
assert.deepEqual(filterMissionRows(searchable, 'working ship').map(row => row.mission.id), ['mission/demo/ship']);
assert.deepEqual(filterMissionRows(searchable, 'ship idle'), []);
assert.deepEqual(filterMissionRows(searchable, '  '), searchable);
