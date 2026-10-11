import assert from 'node:assert/strict';
import { homeRows, homeSections, keepClosed, onHome } from '@smalltalk/st3-views/homeView';

const now = Date.parse('2026-09-30T12:00:00Z');
const item = (id, attention_kind, extra = {}) => ({ id: `attention/${id}`, kind: 'attention', attention_kind, title: id, detail: '', priority: 'normal', state: 'open', person_id: 'person/alex', requested_at: '2026-09-30T11:00:00Z', source_id: 'x', actions: [], ...extra });

// Home shows what stui's Home shows: the person's unresolved requests and reviews, items without
// app actions included. A message, a fault and a kind st adds later never reach Home.
const items = [
  item('note', 'unread-message'),
  item('fault', 'fault'),
  item('gate', 'human-gate', { actions: ['review.approve'], step_run_id: 'step-run/abc/build' }),
  item('launch', 'launch-approval'),
  item('broke', 'fault', { priority: 'critical' }),
  item('ask', 'agent-request'),
  item('step', 'person-step', { step_run_id: 'step-run/abc/approve-copy' }),
  item('done', 'human-gate', { state: 'resolved' }),
  item('theirs', 'human-gate', { person_id: 'person/someone-else' }),
  item('new-kind', 'something-new'),
];
const rows = homeRows(items, 'person/alex', now);
assert.deepEqual(rows.map(row => row.item.id.replace('attention/', '')), ['gate', 'ask', 'step', 'launch']);
assert.deepEqual(homeSections(rows).map(section => [section.title, section.count]), [['somebody is stopped on you', 3], ['today', 1]]);
assert.deepEqual([rows[0].kind, rows[0].glyph, rows[0].waiting, rows[0].age], ['review', '◆', 'step build', '1h']);
assert.deepEqual([rows[2].kind, rows[2].waiting], ['request', 'step approve-copy']);

// Before the connection names its person, nothing is hidden for belonging to someone else.
assert.equal(onHome(item('theirs', 'human-gate', { person_id: 'person/other' }), undefined), true);
assert.equal(onHome(item('done', 'human-gate', { state: 'resolved' }), undefined), false);

// A paired phone acts as person/NAME/session/ID: the person's items are still its own.
assert.equal(onHome(item('mine', 'review', { person_id: 'person/alex' }), 'person/alex/session/0190abcd'), true);
assert.equal(onHome(item('theirs', 'review', { person_id: 'person/robin' }), 'person/alex/session/0190abcd'), false);

// An update brings what the person asked for: it sits under "when there is time", after anything
// that waits on them, and asks nothing.
const withUpdate = homeRows([
  item('update', 'person-step', { update: { version: 1, type: 'update', about: 'message/0123456789abcdef' } }),
  item('ask', 'person-step'),
], 'person/alex', now);
assert.deepEqual(withUpdate.map(row => [row.item.id.replace('attention/', ''), row.kind, row.glyph]), [['ask', 'request', '◆'], ['update', 'update', '✦']]);
assert.deepEqual(homeSections(withUpdate).map(section => section.title), ['somebody is stopped on you', 'when there is time']);

// Renderers choose actual colors; the shared model describes their meaning.
assert.deepEqual(withUpdate.map(row => row.color), ['person', 'green']);

// Nothing leaves Home by itself: an item st closes (or resolves) while it is shown stays, marked.
{
  const open = id => ({ id, state: 'open', attention_kind: 'person-step', actions: ['work.done'], title: id, requested_at: '2026-10-07T07:00:00Z', update: { summary: 'x', about: 'y' } });
  const first = [open('attention/a'), open('attention/b'), open('attention/c')];
  // a is gone, b is resolved, c was answered here.
  const next = [{ ...open('attention/b'), state: 'resolved' }];
  const kept = keepClosed(first, next, new Set(['attention/c']));
  assert.deepEqual(kept.map(item => item.id).sort(), ['attention/a', 'attention/b']);
  assert.ok(kept.every(item => item.closedElsewhere && item.actions.length === 0));
  const rows = homeRows(kept, undefined, Date.parse('2026-10-07T07:05:00Z'));
  assert.equal(rows.length, 2, 'a resolved copy is replaced by the open one that was shown');
  assert.match(rows[0].waiting, /closed; stays/);
  const sections = homeSections(homeRows([open('attention/open'), ...kept], undefined, Date.parse('2026-10-07T07:05:00Z')));
  assert.deepEqual(sections.map(section => [section.title, section.count]), [['when there is time', 1], ['Recently closed: clear each', 2]], 'closed items have a section of their own');
  // Again, with the marked ones as the previous list: they stay.
  assert.equal(keepClosed(kept, next, new Set()).length, 2);
  // An item that was never open on Home is not invented.
  assert.deepEqual(keepClosed([{ ...open('attention/z'), state: 'resolved' }], [], new Set()), []);
  // Only the latest five stay listed; the oldest drops first.
  const all = [0, 1, 2, 3, 4, 5, 6, 7].map(n => open(`attention/${n}`));
  let list = all;
  for (let n = 0; n < 7; n++) list = keepClosed(list, all.slice(n + 1), new Set());
  assert.deepEqual(list.filter(item => item.closedElsewhere).map(item => item.id).sort(), ['attention/2', 'attention/3', 'attention/4', 'attention/5', 'attention/6']);
  // An item that comes back open loses its mark.
  assert.equal(keepClosed(kept, [open('attention/a')], new Set()).find(item => item.id === 'attention/a').closedElsewhere, undefined);
}

// A condition is visible without controls and clears through measured recovery.
{
  const rows = homeRows([item('disk', 'condition'),item('empty', 'condition', {title:''})], 'person/alex', now);
  assert.equal(rows.length, 2);
  assert.deepEqual(rows.map(row => [row.kind,row.tier,row.item.actions]), [['condition','today',[]],['condition','today',[]]]);
  assert.equal(rows[1].title, 'Condition breached');
}

const blankCondition = homeRows([item('blank-condition', 'condition', { title: '   ' })], 'person/alex')[0];
assert.equal(blankCondition.title, 'Condition breached');
assert.equal(blankCondition.glyph, '!');
