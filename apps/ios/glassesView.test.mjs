import assert from 'node:assert/strict';
import { glassChoices, glassGroups, groups, paneTarget } from './glassesView.ts';

const agent = (id, name) => ({ id, kind: 'agent', name, state: 'running' });
const mission = (id, title) => ({ id, kind: 'mission', title, state: 'running' });
const lists = {
  agents: [agent('agent/example/harbor/keeper', 'example/harbor/keeper')],
  missions: [mission('mission/example/harbor/audit', 'example/harbor/audit')],
  attention: [
    { id: 'attention/a', state: 'open', mission_id: 'mission/example/harbor/audit', source_id: 'step-run/x/review' },
    { id: 'attention/b', state: 'resolved', requester_id: 'agent/example/harbor/keeper', source_id: 'x' },
  ],
  machines: [{ id: 'machine/lighthouse', kind: 'machine', name: 'lighthouse', state: 'local' }],
};
const glass = (id, name, layout, extra = {}) => ({ id: `glass/person/avery/${id}`, kind: 'glass', revision: 'r1', updated_at: '', deleted: false, body: { name, layout }, ...extra });
const split = (split, a, b) => ({ split, children: [a, b] });
const group = (...tabs) => ({ tabs: tabs.map(tab => typeof tab === 'string' ? { pane: tab } : tab) });

// Splits read left to right, top to bottom, as stui numbers them.
assert.deepEqual(groups(split('right', group('a'), split('below', group('b', 'c'), group()))).map(tabs => tabs.map(tab => tab.pane)), [['a'], ['b', 'c'], []]);

// Choices are the live glasses by name.
assert.deepEqual(glassChoices([glass('2', 'review', group()), glass('1', 'main', group()), glass('3', 'old', group(), { deleted: true, body: null })]).map(choice => choice.name), ['main', 'review']);

// A pane is named as the app names its subject, and says when it needs the person or is gone.
const keeper = paneTarget('agent:agent/example/harbor/keeper', lists);
assert.deepEqual([keeper.kind, keeper.title, keeper.needsYou, keeper.gone], ['agent', 'Keeper', false, false], 'a resolved item waits on nobody');
const audit = paneTarget('mission:mission/example/harbor/audit', lists);
assert.deepEqual([audit.kind, audit.title, audit.needsYou], ['mission', 'Audit', true]);
assert.equal(paneTarget('agent:agent/example/retired', lists).gone, true);
assert.equal(paneTarget('machine:machine/lighthouse', lists).title, 'lighthouse');
assert.equal(paneTarget('machine:machine/elsewhere', { ...lists, machines: [] }).gone, false, 'machines not loaded yet are not gone');
assert.deepEqual([paneTarget('later-kind:thing', lists).kind, paneTarget('later-kind:thing', lists).title], ['other', 'later-kind:thing']);

// Each split's tabs: their own title or their pane's, ◆ when the pane needs the person.
const shown = glassGroups(glass('1', 'main', split('right',
  group('agent:agent/example/harbor/keeper'),
  group('mission:mission/example/harbor/audit', { title: 'hosts', pane: 'machine:machine/lighthouse' }),
)), lists);
assert.deepEqual(shown.map(row => row.tabs.map(tab => [tab.group, tab.title, tab.pane.needsYou])), [
  [[0, 'Keeper', false]],
  [[1, 'Audit', true], [1, 'hosts', false]],
]);
assert.deepEqual(glassGroups(glass('1', 'main', group()), lists), [{ index: 0, tabs: [] }], 'a glass may be Home alone');
