import assert from 'node:assert/strict';
import { alertConversations, alertsHeading, alertsIn, answerLabel, homeRows, isAlert, offeredAnswers, openAlerts, pendingCalls, pendingQuestions, promptAlertKey, promptAnswers, promptCalls, questionAnswer } from '@smalltalk/st3-views/homeView';

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
const prompt = { actions: ['prompt.respond'], source_id: 'agent/example/ada', episode: 'ep1', action_parameters: { 'prompt.respond': { target_id: 'agent/example/ada', episode: 'ep7', answers: ['allow', 'deny', 42, 'Not An Answer'] } } };
assert.deepEqual(promptAnswers(prompt), { target_id: 'agent/example/ada', episode: 'ep7', answers: ['allow', 'deny'] });
// A screen prompt's own answers come through as they are.
assert.deepEqual(promptAnswers({ ...prompt, action_parameters: { 'prompt.respond': { episode: 'ep8', answers: ['use', 'use-all', 'skip'] } } }).answers, ['use', 'use-all', 'skip']);
assert.equal(promptAnswers({ ...prompt, actions: [] }), null);
assert.equal(promptAnswers({ ...prompt, action_parameters: {} }), null);
assert.equal(promptAnswers({ ...prompt, action_parameters: { 'prompt.respond': { answers: [] } } }), null);

// Every tool call with no result yet, in full, oldest first.
const call = (id, name, args) => ({ id: `entry-${id}`, type: 'tool_call', body: { call_id: id, name, arguments: args } });
const result = id => ({ id: `result-${id}`, type: 'tool_result', body: { call_id: id, status: 'ok', content: 'ok' } });
assert.deepEqual(pendingCalls([]), []);
assert.deepEqual(pendingCalls([call('c1', 'Bash', { command: 'ls' }), result('c1')]), []);
assert.deepEqual(pendingCalls([call('c1', 'Bash', { command: 'ls' }), result('c1'), call('c2', 'Bash', { command: 'rm -rf build\necho done', description: 'Clean' })]), [
  { id: 'c2', tool: 'Bash', lines: ['command: rm -rf build', '  echo done', 'description: Clean'], input: { command: 'rm -rf build\necho done', description: 'Clean' } },
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

// A question is answered with its choices: each question by its text, several labels joined.
const asked = [call('q1', 'AskUserQuestion', { questions: [
  { question: 'Which color?', header: 'Color', multiSelect: false, options: [{ label: 'Red', description: '' }, { label: 'Blue', description: '' }] },
  { question: 'Which pets?', header: 'Pets', multiSelect: true, options: [{ label: 'Cat', description: '' }, { label: 'Dog', description: '' }, { label: 'Fish', description: '' }] },
] })];
const questions = pendingQuestions(pendingCalls(asked));
assert.equal(questions.length, 2);
assert.equal(questions[1].multiSelect, true);
assert.equal(questionAnswer(questions, [[1]]), null, 'every question needs a choice');
// Positions only: the questions' and options' words stay in the conversation.
assert.deepEqual(JSON.parse(questionAnswer(questions, [[1], [2, 0]])), { 0: [1], 1: [0, 2] });
assert.equal(questionAnswer(questions, [[1], [7]]), null, 'an option the question does not have');
// Choices belong to one prompt: a new episode or revision behind the same alert is another key.
const prompting = (episode, revision) => item('p', 'harness-prompt', { revision, actions: ['prompt.respond'], action_parameters: { 'prompt.respond': { target_id: 'x', episode, answers: ['answer', 'deny'] } } });
assert.equal(promptAlertKey(prompting('e1', 'r1')), promptAlertKey(prompting('e1', 'r1')));
assert.notEqual(promptAlertKey(prompting('e2', 'r1')), promptAlertKey(prompting('e1', 'r1')), 'a new prompt behind the alert');
assert.notEqual(promptAlertKey(prompting('e1', 'r2')), promptAlertKey(prompting('e1', 'r1')), 'a new revision of it');
assert.equal(pendingQuestions(pendingCalls([call('b', 'Bash', { command: 'ls' })])), null);
assert.equal(pendingQuestions([]), null);
const question = { target_id: 'a', episode: 'e', answers: ['answer', 'deny'] };
assert.deepEqual(offeredAnswers(question, pendingCalls(asked)), ['answer', 'deny']);
assert.deepEqual(offeredAnswers(question, []), ['deny'], 'no question to choose from yet');
// A plan is approved only when it is shown; a screen prompt's answers are always offered.
const plan = { target_id: 'a', episode: 'e', answers: ['approve', 'deny'] };
assert.deepEqual(offeredAnswers(plan, pendingCalls([call('p', 'ExitPlanMode', { plan: '1. Build' })])), ['approve', 'deny']);
assert.deepEqual(offeredAnswers(plan, []), ['deny']);
assert.deepEqual(offeredAnswers({ target_id: 'a', episode: 'e', answers: ['trust', 'exit'] }, []), ['trust', 'exit']);
assert.equal(answerLabel('use-all'), 'Use all');
assert.equal(answerLabel('approve'), 'Approve plan');
// A call auto mode denied: allow it once or its kind, or leave it denied; none needs a shown call.
assert.deepEqual(offeredAnswers({ target_id: 'a', episode: 'e', answers: ['approve-once', 'approve-kind', 'dismiss'] }, []), ['approve-once', 'approve-kind', 'dismiss']);
assert.equal(answerLabel('approve-kind'), 'Allow this kind');

// A prompt names its exact call: allow is offered only beside it, never beside another pending
// call (a parent's Task call while its subagent asks), and not at all when it named none.
const bound = { target_id: 'a', episode: 'e', answers: ['allow', 'deny'], call_id: 'c2' };
const parentAndChild = pendingCalls([call('c1', 'Task', { prompt: 'run the build' })]);
assert.deepEqual(offeredAnswers(bound, parentAndChild), ['deny'], 'the pending call is not the one asked about');
const both = pendingCalls([call('c1', 'Task', { prompt: 'run the build' }), call('c2', 'Bash', { command: 'make' })]);
assert.deepEqual(offeredAnswers(bound, both), ['allow', 'deny']);
assert.deepEqual(promptCalls(bound, both).map(each => each.id), ['c2']);
assert.deepEqual(offeredAnswers({ ...bound, call_id: null }, both), ['deny'], 'no named call, no allow');
assert.equal(promptAnswers({ ...prompt, action_parameters: { 'prompt.respond': { episode: 'e', answers: ['allow'], call_id: 'c2' } } }).call_id, 'c2');
assert.equal(promptAnswers({ ...prompt, action_parameters: { 'prompt.respond': { episode: 'e', answers: ['allow'], call_id: null } } }).call_id, null);
assert.equal(promptAnswers({ ...prompt, action_parameters: { 'prompt.respond': { episode: 'e', answers: ['allow'] } } }).call_id, undefined);
