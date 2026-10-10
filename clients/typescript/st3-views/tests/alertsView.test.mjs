import assert from 'node:assert/strict';
import { alertConversations, alertsHeading, alertsIn, homeRows, isAlert, offeredAnswers, openAlerts, pendingCalls, promptAnswers } from '@smalltalk/st3-views/homeView';

const item = (id, attention_kind, extra = {}) => ({ id: `attention/${id}`, kind: 'attention', attention_kind, title: id, detail: '', priority: 'normal', state: 'open', person_id: 'person/alex', requested_at: '2026-09-30T11:00:00Z', source_id: 'x', actions: [], ...extra });
const actor = 'person/alex/session/phone';

// Zero prints nothing; one is singular.
assert.equal(alertsHeading(0), '');
assert.equal(alertsHeading(1), '1 alert');
assert.equal(alertsHeading(3), '3 alerts');

// The daemon says; a daemon that predates alerts is read as everything but an update or an unread message.
assert.equal(isAlert(item('a', 'person-step', { alert: false })), false);
assert.equal(isAlert(item('a', 'person-step', { alert: true })), true);
assert.equal(isAlert(item('a', 'person-step')), true);
assert.equal(isAlert(item('a', 'person-step', { update: { summary: 'x', about: 'y' } })), false);
assert.equal(isAlert(item('a', 'unread-message')), false);

const items = [
  item('ask', 'person-step', { alert: true, source_id: 'step-run/r/ask', conversation_id: 'agent/example/ada' }),
  item('update', 'person-step', { alert: false, update: { summary: 'x', about: 'y' }, conversation_id: 'agent/example/ada' }),
  item('gate', 'human-gate', { alert: true, conversation_id: 'agent/example/bea' }),
  item('prompt', 'harness-prompt', { alert: true, source_id: 'agent/example/ada', conversation_id: 'agent/example/ada', actions: ['prompt.respond'] }),
  item('login', 'harness-login', { alert: true, source_id: 'login/host', conversation_id: 'agent/example/ada', conversation_ids: ['agent/example/ada', 'agent/example/bea'] }),
  item('note', 'unread-message', { alert: false }),
  item('theirs', 'human-gate', { alert: true, person_id: 'person/someone-else' }),
  item('closed', 'human-gate', { alert: true, closedElsewhere: true }),
  item('fault', 'fault', { alert: true }),
];
// The count is what Home lists: the same rows.
const open = openAlerts(items, actor);
assert.deepEqual(open.map(row => row.id.replace('attention/', '')), ['ask', 'gate', 'prompt', 'login']);
assert.deepEqual(homeRows(items, actor).filter(row => !row.item.closedElsewhere && row.kind !== 'update').map(row => row.item.id), open.map(row => row.id));
assert.deepEqual(homeRows(items, actor).filter(row => row.item.id.endsWith('prompt') || row.item.id.endsWith('login')).map(row => row.kind), ['prompt', 'login']);

// Each alert shows in its agent's conversation; a login in every seat it covers, once each.
const ids = agent => alertsIn(items, agent, actor).map(row => row.id.replace('attention/', ''));
assert.deepEqual(ids('agent/example/ada'), ['ask', 'prompt', 'login']);
assert.deepEqual(ids('agent/example/bea'), ['gate', 'login']);
assert.deepEqual(ids('agent/example/nobody'), []);
assert.deepEqual(alertConversations({ source_id: 's', conversation_id: 'agent/a', conversation_ids: ['agent/a', 'agent/b'] }), ['agent/a', 'agent/b']);
// An older daemon names none: the agent that asked, else the seat itself.
assert.deepEqual(alertConversations({ source_id: 'step-run/r/s', requester_id: 'agent/asker' }), ['agent/asker']);
assert.deepEqual(alertConversations({ source_id: 'agent/seat' }), ['agent/seat']);
assert.deepEqual(alertConversations({ source_id: 'step-run/r/s' }), []);

// A native prompt's answers come from its alert; nothing when it offers none.
const prompt = { actions: ['prompt.respond'], source_id: 'agent/example/ada', episode: 'ep1', action_parameters: { 'prompt.respond': { target_id: 'agent/example/ada', episode: 'ep7', answers: ['allow', 'deny', 'maybe'] } } };
assert.deepEqual(promptAnswers(prompt), { target_id: 'agent/example/ada', episode: 'ep7', answers: ['allow', 'deny'] });
assert.equal(promptAnswers({ ...prompt, actions: [] }), null);
assert.equal(promptAnswers({ ...prompt, action_parameters: {} }), null);
assert.equal(promptAnswers({ ...prompt, action_parameters: { 'prompt.respond': { answers: [] } } }), null);

// Every tool call with no result yet, in full, oldest first.
const call = (id, name, args) => ({ id: `entry-${id}`, type: 'tool_call', body: { call_id: id, name, arguments: args } });
const result = id => ({ id: `result-${id}`, type: 'tool_result', body: { call_id: id, status: 'ok', content: 'ok' } });
assert.deepEqual(pendingCalls([]), []);
assert.deepEqual(pendingCalls([call('c1', 'Bash', { command: 'ls' }), result('c1')]), []);
assert.deepEqual(pendingCalls([call('c1', 'Bash', { command: 'ls' }), result('c1'), call('c2', 'Bash', { command: 'rm -rf build\necho done', description: 'Clean' })]), [
  { id: 'c2', tool: 'Bash', lines: ['command: rm -rf build', '  echo done', 'description: Clean'] },
]);
assert.deepEqual(pendingCalls([call('c3', 'Edit', JSON.stringify({ file_path: '/tmp/a', old_string: 'x' }))])[0].lines, ['file_path: /tmp/a', 'old_string: x']);
// Two unanswered calls are both listed.
const two = pendingCalls([call('c4', 'Bash', { command: 'make' }), result('c9'), call('c5', 'Write', { file_path: '/tmp/b' })]);
assert.deepEqual(two.map(each => each.id), ['c4', 'c5']);
// Allow is offered only when exactly one call is unanswered and shown in full; deny always.
const answers = { target_id: 'a', episode: 'e', answers: ['allow', 'deny'] };
assert.deepEqual(offeredAnswers(answers, []), ['deny']);
assert.deepEqual(offeredAnswers(answers, [{ id: 'c', tool: 'Bash', lines: [] }]), ['deny']);
assert.deepEqual(offeredAnswers(answers, [{ id: 'c', tool: 'Bash', lines: ['command: ls'] }]), ['allow', 'deny']);
assert.deepEqual(offeredAnswers(answers, two), ['deny']);
