import assert from 'node:assert/strict';
import { glassChoices, glassTabs, leaves, paneTarget } from './glassesView.ts';

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
const glass = (id, name, tabs, extra = {}) => ({ id: `glass/person/avery/${id}`, kind: 'glass', revision: 'r1', updated_at: '', deleted: false, body: { name, tabs }, ...extra });
const split = (split, a, b) => ({ split, children: [a, b] });
const pane = key => ({ pane: key });

// Leaves read left to right, top to bottom, as stui numbers them.
assert.deepEqual(leaves(split('right', pane('a'), split('below', pane('b'), pane('c')))), ['a', 'b', 'c']);

// Choices are the live glasses by name.
assert.deepEqual(glassChoices([glass('2', 'review', []), glass('1', 'main', []), glass('3', 'old', [], { deleted: true, body: null })]).map(choice => choice.name), ['main', 'review']);

// A pane is named as the app names its subject, and says when it needs the person or is gone.
const keeper = paneTarget('agent:agent/example/harbor/keeper', lists);
assert.deepEqual([keeper.kind, keeper.title, keeper.needsYou, keeper.gone], ['agent', 'Keeper', false, false], 'a resolved item waits on nobody');
const audit = paneTarget('mission:mission/example/harbor/audit', lists);
assert.deepEqual([audit.kind, audit.title, audit.needsYou], ['mission', 'Audit', true]);
assert.equal(paneTarget('agent:agent/example/retired', lists).gone, true);
assert.equal(paneTarget('machine:machine/lighthouse', lists).title, 'lighthouse');
assert.equal(paneTarget('machine:machine/elsewhere', { ...lists, machines: [] }).gone, false, 'machines not loaded yet are not gone');
assert.deepEqual([paneTarget('later-kind:thing', lists).kind, paneTarget('later-kind:thing', lists).title], ['other', 'later-kind:thing']);

// Tabs: their own title, or the first pane and how many more; ◆ when any pane needs the person.
const tabs = glassTabs(glass('1', 'main', [
  { layout: pane('agent:agent/example/harbor/keeper') },
  { layout: split('right', pane('mission:mission/example/harbor/audit'), pane('machine:machine/lighthouse')) },
  { title: 'the audit', layout: pane('mission:mission/example/harbor/audit') },
]), lists);
assert.deepEqual(tabs.map(tab => [tab.title, tab.panes.length, tab.needsYou]), [['Keeper', 1, false], ['Audit +1', 2, true], ['the audit', 1, true]]);
assert.deepEqual(glassTabs(glass('1', 'main', []), lists), [], 'a glass may be Home alone');
