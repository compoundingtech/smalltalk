import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { decodeWorld } from './clientView.ts';
import { markdown } from './markdown.ts';
import { aboutText, aboutTitle, agentSections, agentsTree, chatTarget, flowLayers, homeBadge, homeSections, listState, missionHelp, missionSections, reconcilePending, withPending } from './screenModel.ts';
import { agentLabel, expiresIn, missionDisplayLabel, notLoaded, personOf, preview, world } from './worldAdapter.ts';

const demo = decodeWorld(JSON.parse(readFileSync(new URL('../../fixtures/clients/demo-world.json', import.meta.url), 'utf8')));
const noExtras = { conversations: {}, previews: {}, bodies: {}, live: true, offline: null, worktrees: { state: 'ready', value: [] } };

// Home groups by tier in order, and the badge turns person-coloured when someone is stopped.
assert.deepStrictEqual(homeSections(demo, new Set()).map(section => [section.title, section.count]), [
  ['somebody is stopped on you', 2], ['something broke', 1], ['today', 2], ['when there is time', 1],
]);
assert.equal(homeSections(demo, new Set(['attention/6'])).length, 3);
assert.deepStrictEqual(homeBadge(demo), { count: 6, stopped: true });

// Agents group by who has to move, unmanaged last; the tree starts at the first path folder.
assert.deepStrictEqual(agentSections(demo).map(section => section.title), ['waiting on you', 'broken', 'working', 'idle', 'stopped', 'found running · not started by st']);
assert.deepStrictEqual(agentsTree(demo)[0], { kind: 'folder', depth: 0, name: 'fleet/', key: 'fleet' });
assert.ok(agentsTree(demo).filter(row => row.kind === 'leaf').length === 10);

// Missions group by their word in the contract's order.
assert.deepStrictEqual(missionSections(demo, false).map(section => section.title), ['needs you', 'stalled', 'queued', 'working', 'idle', 'done']);

// A stalled mission explains what a person can do; a queued one says there is nothing to do.
const stalled = missionHelp(demo, demo.missions.value.find(mission => mission.word === 'stalled'));
assert.equal(stalled.kind, 'stuck');
assert.equal(stalled.broken?.id, 'agent/fleet/release/captain');
assert.equal(missionHelp(demo, demo.missions.value.find(mission => mission.word === 'queued')).kind, 'queued');

// Steps lay out as a flow of layers.
assert.deepStrictEqual(flowLayers([
  { name: 'scan', after: [] }, { name: 'fix', after: ['scan'] }, { name: 'review', after: ['fix'] }, { name: 'lint', after: ['fix'] }, { name: 'merge', after: ['review', 'lint'] },
]).map(layer => layer.map(step => step.name)), [['scan'], ['fix'], ['review', 'lint'], ['merge']]);

// Chat about this goes to the agent involved, or to the chief of staff.
const fault = demo.attention.value.find(item => item.kind.kind === 'fault');
assert.equal(chatTarget(demo, fault).id, 'agent/fleet/release/captain');
assert.equal(chatTarget(demo, { ...fault, agent: null }).id, 'agent/fleet/cos');
assert.equal(aboutTitle(fault), 'About: Release Captain keeps restarting');
assert.match(aboutText(fault, 'Why?'), /^Why\?\n\n---\nThis is about Release Captain keeps restarting \(attention\/4, mission mission\/fleet\/release\/weekly\)$/);

// A pending send stays until its message id appears in the conversation, then gives way.
{
  const pending = [{ token: 't', agent: 'agent/a', text: 'hi', at: '09:00', messageId: 'message/9', failed: null }];
  assert.equal(reconcilePending(pending, 'agent/a', new Set(['message/1'])).length, 1);
  assert.equal(reconcilePending(pending, 'agent/a', new Set(['message/9'])).length, 0);
  assert.equal(reconcilePending([{ ...pending[0], messageId: null }], 'agent/a', new Set(['message/9'])).length, 1);
  const shown = withPending({ 'agent/a': { state: 'ready', value: [] } }, pending);
  assert.deepStrictEqual(shown['agent/a'].value[0].body, { kind: 'pending', value: { text: 'hi', failed: null } });
}

// Loading, empty and failed stay three different things.
assert.equal(listState({ state: 'loading' }, 'Loading…', 'None').kind, 'loading');
assert.equal(listState({ state: 'ready', value: [] }, 'Loading…', 'None').kind, 'empty');
assert.equal(listState({ state: 'failed', value: 'gone' }, 'Loading…', 'None').kind, 'failed');

// Labels read like names.
assert.equal(agentLabel({ name: 'fleet/stui/ios-parity/2026-09-28/ios-builder' }), 'iOS Builder');
assert.equal(agentLabel({ name: 'fleet/harbor/omp' }), 'Harbor · OMP');
assert.equal(missionDisplayLabel({ id: 'mission/fleet/stui/ios-parity', title: 'fleet/stui/ios-parity' }), 'fleet/stui · iOS Parity');

// A launch preview reads steps, assignees, dependencies and person gates.
{
  const result = preview('harbor/audit', {
    goals: ['Audit dependencies'],
    display_order: ['scan', 'merge'],
    steps: {
      scan: { path: 'scan', work_selector: { kind: 'assigned', agent: 'agent/fleet/auditor' }, dependencies: [], gates: [] },
      merge: { path: 'merge', work_selector: { kind: 'agentless' }, dependencies: [{ dependency: 'step', step: 'scan', state: 'completed' }], gates: [{ reviewer: 'person/robin' }] },
    },
  });
  assert.deepStrictEqual(result.goals, ['Audit dependencies']);
  assert.equal(result.steps[0].assignee, 'fleet/auditor');
  assert.deepStrictEqual(result.steps[1].after, ['scan']);
  assert.equal(result.steps[1].asks_you, true);
  assert.equal(result.agents.length, 1);
}

// The live adapter: unloaded is loading, a failed read says why, and mission words come from work.
{
  const now = Date.parse('2026-09-28T12:00:00Z');
  const at = '2026-09-28T11:30:00Z';
  const graph = {
    actor: 'person/robin', hostId: 'host/lark',
    attention: notLoaded(), agents: { items: [], loaded: false, error: 'forbidden: agents' }, missions: notLoaded(), work: notLoaded(),
    machines: notLoaded(), runtimes: notLoaded(), sessions: notLoaded(), devices: notLoaded(),
  };
  const empty = world(graph, noExtras, now);
  assert.equal(empty.attention.state, 'loading');
  assert.deepStrictEqual(empty.agents, { state: 'failed', value: 'forbidden: agents' });
  assert.equal(empty.host, 'lark');

  const mission = (id, runs) => ({ id, kind: 'mission', revision: '1', updated_at: at, title: id.slice('mission/'.length), state: 'running', runs, run_generations: {}, mission_revision: '1' });
  const work = (id, run, path, state, extra = {}) => ({ id, kind: 'work', revision: '1', updated_at: at, attempt: 1, constraints: [], goals: ['Do it'], definition_id: 'd', generation_id: 'g', mission_run_id: run, path, readiness_epoch: 1, state, ...extra });
  const agent = (id, state, extra = {}) => ({ id, kind: 'agent', revision: '1', updated_at: at, name: id.slice('agent/'.length), reachability: 'local', runtime_ids: [], state, ...extra });
  const live = world({
    ...graph,
    attention: { items: [], loaded: true },
    missions: { items: [mission('mission/fleet/watch', ['run/w']), mission('mission/fleet/busy', ['run/b']), mission('mission/fleet/orphan', ['run/o'])], loaded: true },
    work: { items: [
      work('step-run/w1', 'run/w', 'keep-watch', 'claimed', { agentless: true }),
      work('step-run/b1', 'run/b', 'build', 'ready'),
      work('step-run/o1', 'run/o', 'fix', 'ready'),
      work('step-run/x', 'run/x', 'other', 'claimed'),
    ], loaded: true },
    agents: { items: [
      agent('agent/fleet/builder', 'running', { current_work_ids: ['step-run/x'], upcoming_work_ids: ['step-run/b1'], harness_state: 'working', runtime_ids: ['runtime/b'] }),
      agent('agent/fleet/parked', 'stopped', { runtime_ids: ['runtime/p'] }),
    ], loaded: true },
    devices: { items: [{ id: 'device/phone', kind: 'device', revision: '1', updated_at: at, name: 'Phone', person_id: 'person/robin', session_actor: 'person/robin', scopes: ['full control'], state: 'active', expires_at: '2026-10-08T12:00:00Z' }], loaded: true },
  }, noExtras, now);
  const words = Object.fromEntries(live.missions.value.map(item => [item.id, item.word]));
  assert.deepStrictEqual(words, { 'mission/fleet/watch': 'watching', 'mission/fleet/busy': 'queued', 'mission/fleet/orphan': 'unclaimed' });
  const busy = live.missions.value.find(item => item.id === 'mission/fleet/busy');
  assert.equal(busy.steps[0].note, 'queued for Builder, which is busy with run/x › other');
  assert.equal(live.missions.value.find(item => item.id === 'mission/fleet/watch').steps[0].owner, 'st');
  assert.equal(live.agents.value[0].state, 'working');
  assert.equal(live.agents.value[0].activity, '30m');
  // A running agent with a runtime may have a terminal even when the bounded runtime list lacks it.
  assert.deepStrictEqual(live.agents.value.map(item => item.terminal), [true, false]);
  assert.deepStrictEqual(live.devices, { state: 'ready', value: [{ id: 'device/phone', name: 'Phone', state: 'active', scopes: ['full control'], expires: 'in 10 days' }] });
  assert.equal(expiresIn('2026-09-26T12:00:00Z', now), '2 days ago');
}

// Markdown keeps lines, and reads headings, bullets, code, tables and inline marks.
assert.deepStrictEqual(markdown('# Title\n\n- **one** `two`\n```sh\nls\n```\n| a | b |\n|---|---|\n| 1 | 2 |\n'), [
  { kind: 'heading', level: 1, runs: [{ text: 'Title' }] },
  { kind: 'blank' },
  { kind: 'item', indent: 0, marker: '• ', runs: [{ text: 'one', bold: true }, { text: ' ' }, { text: 'two', code: true }] },
  { kind: 'code', language: 'sh', lines: ['ls'] },
  { kind: 'table', rows: [['a', 'b'], ['1', '2']] },
]);

// A paired device acts as `person/…/session/…`; attention is addressed to the person.
assert.equal(personOf('person/robin/session/abc', [{ session_actor: 'person/robin/session/abc', person_id: 'person/robin' }]), 'person/robin');
assert.equal(personOf('person/robin/session/abc', []), 'person/robin');
assert.equal(personOf('person/robin', []), 'person/robin');
