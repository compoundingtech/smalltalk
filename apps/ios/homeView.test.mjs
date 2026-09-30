import assert from 'node:assert/strict';
import { homeRows, homeSections, onHome } from './homeView.ts';

const now = Date.parse('2026-09-30T12:00:00Z');
const item = (id, attention_kind, extra = {}) => ({ id: `attention/${id}`, kind: 'attention', attention_kind, title: id, detail: '', priority: 'normal', state: 'open', person_id: 'person/nathan', requested_at: '2026-09-30T11:00:00Z', source_id: 'x', actions: [], ...extra });

// Home shows what stui's Home shows: every unresolved item for this person, unread messages and
// items without app actions included.
const items = [
  item('note', 'unread-message'),
  item('fault', 'fault'),
  item('gate', 'human-gate', { actions: ['review.approve'], step_run_id: 'step-run/abc/build' }),
  item('launch', 'launch-approval'),
  item('broke', 'fault', { priority: 'critical' }),
  item('ask', 'agent-request'),
  item('done', 'fault', { state: 'resolved' }),
  item('theirs', 'fault', { person_id: 'person/someone-else' }),
  item('new-kind', 'something-new'),
];
const rows = homeRows(items, 'person/nathan', now);
assert.deepEqual(rows.map(row => row.item.id.replace('attention/', '')), ['gate', 'ask', 'broke', 'fault', 'launch', 'new-kind', 'note']);
assert.deepEqual(homeSections(rows).map(section => [section.title, section.count]), [['somebody is stopped on you', 2], ['something broke', 1], ['today', 3], ['when there is time', 1]]);
assert.deepEqual([rows[0].kind, rows[0].glyph, rows[0].waiting, rows[0].age], ['review', '◆', 'step build', '1h']);
assert.equal(rows.at(-1).glyph, '✉');
assert.equal(rows.find(row => row.item.id === 'attention/new-kind').kind, 'fault');

// Before the connection names its person, nothing is hidden for belonging to someone else.
assert.equal(onHome(item('theirs', 'fault', { person_id: 'person/other' }), undefined), true);
assert.equal(onHome(item('done', 'fault', { state: 'resolved' }), undefined), false);
