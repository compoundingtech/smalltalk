import assert from 'node:assert/strict';
import { glassChoices, glassGroups, groupBoxes, groups, paneTarget } from './glassesView.ts';

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
// Agents and missions carry their state as their own lists mark it (Nathan, 2026-10-02).
assert.equal(keeper.status?.word, 'idle');
const busy = paneTarget('agent:agent/example/harbor/keeper', { ...lists, agents: [{ ...lists.agents[0], harness_state: 'working' }] });
assert.equal(busy.status?.word, 'working');
assert.ok(audit.status?.glyph);
assert.equal(paneTarget('agent:agent/example/retired', lists).status, undefined);
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

// A shell opens in the terminal and a usage tab in Usage; a space says what it holds.
{
  const { paneTarget, spaceSummary } = await import('./glassesView.ts');
  const lists = { agents: [], missions: [], attention: [], machines: [] };
  assert.deepEqual([paneTarget('terminal:terminal/pty/person/robin/abc', lists).kind, paneTarget('usage:agent/x', lists).kind], ['terminal', 'usage']);
  assert.equal(paneTarget('terminal:agent/example/harbor/keeper', lists).kind, 'agent');
  const glass = { id: 'glass/person/robin/1', revision: 'r', body: { name: 'main', layout: { split: 'right', children: [{ tabs: [{ pane: 'agent:agent/a' }, { pane: 'mission:mission/m' }] }, { tabs: [{ pane: 'usage:' }] }] } } };
  assert.equal(spaceSummary(glass), '3 tabs in 2 panes');
  assert.equal(spaceSummary({ ...glass, body: { name: 'one', layout: { tabs: [{ pane: 'agent:agent/a' }] } } }), '1 tab');
}

// A split's ratio sizes its groups, as stui drew them; without one they share evenly.
{
  const sized = { split: 'right', ratio: 0.3, children: [group('a'), { split: 'below', children: [group('b'), group('c')] }] };
  const boxes = groupBoxes(sized).map(box => Object.values(box).map(value => Math.round(value * 100) / 100));
  assert.deepEqual(boxes, [[0, 0, 0.3, 1], [0.3, 0, 0.7, 0.5], [0.3, 0.5, 0.7, 0.5]]);
  assert.deepEqual(groupBoxes(group('a')), [{ x: 0, y: 0, width: 1, height: 1 }]);
}
