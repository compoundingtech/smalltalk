import assert from 'node:assert/strict';
import { homeRows, homeSections, onHome } from '@smalltalk/st3-views/homeView';

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
