import assert from 'node:assert/strict';
import { unreadableTranscript, cleanMessageText, conversationEntries, foldDeliveryFlaps, fromHarness, shownToolLines, toolTitle } from './conversationView.ts';

let sequence = 0;
const at = minute => `2026-09-30T12:${String(minute).padStart(2, '0')}:00Z`;
const e = (type, role, body, minute = sequence) => ({ id: `timeline-entry/${++sequence}`, sequence, revision: 1, final: true, type, role, body, timestamp: at(minute) });
const names = new Map([['agent/example/stui', 'Stui'], ['person/alex', 'you']]);
const paused = 'Native conversation delivery over claude-channel paused while the st daemon was unreachable; the driver stayed online and retried every second.';
const recovered = 'Native conversation delivery over claude-channel recovered and resumed replay from durable graph state.';

const timeline = [
  e('content', 'user', { text: '<system-reminder>ignore me</system-reminder>Please fix it' }, 1),
  e('content', 'assistant', { text: '**On it.**' }, 2),
  e('tool_call', 'assistant', { call_id: 'c1', name: 'Bash', arguments: { command: 'cargo test\n--all' } }, 3),
  e('tool_result', 'tool', { call_id: 'c1', status: 'success', content: [{ type: 'text', text: 'ok 1\nok 2' }] }, 4),
  e('message', 'user', { message_id: 'message/abc', from: 'agent/example/cos/standing/cos', to: 'agent/example/stui', title: 'Hello' }, 5),
  e('content', 'user', { text: 'Body text [id:message/abc]' }, 5),
  e('status', 'system', { status: 'running' }, 6),
  e('usage', 'system', { input_tokens: 1 }, 6),
  e('error', 'system', { code: 'native-delivery-degraded', message: paused, retryable: true, details: { severity: 'warning' } }, 7),
  e('error', 'system', { code: 'native-delivery-recovered', message: recovered, retryable: true, details: { severity: 'warning' } }, 8),
  e('error', 'system', { code: 'native-delivery-degraded', message: paused, retryable: true, details: { severity: 'warning' } }, 9),
  e('error', 'system', { code: 'native-delivery-recovered', message: recovered, retryable: true, details: { severity: 'warning' } }, 10),
  e('error', 'system', { code: 'boom', message: 'the harness exited', retryable: false, details: { severity: 'error' } }, 11),
  e('brand-new-kind', 'system', { text: 'something st added later' }, 12),
];
const describe = entry => entry.body.kind === 'event' ? `event(${entry.body.tone}): ${entry.body.text}`
  : entry.body.kind === 'tool' ? `tool(${entry.body.state}): ${entry.body.title} [${entry.body.output.join('|')}]`
  : entry.body.kind === 'mail' ? `mail: ${entry.body.from} → ${entry.body.to} · ${entry.body.subject} · ${entry.body.text}`
  : `${entry.body.kind}: ${entry.body.text}`;
const entries = conversationEntries(timeline, names);
assert.deepEqual(entries.map(describe), [
  'user: Please fix it',
  'assistant: **On it.**',
  'tool(ok): $ cargo test [ok 1|ok 2]',
  'mail: example/cos/standing/cos → Stui · Hello · Body text',
  'event(quiet): message delivery paused 2 times while st restarted · recovered',
  'event(fault): error: the harness exited',
  'event(quiet): something st added later',
]);
assert.equal(entries[0].at.length, 5);

// A pause that has not recovered yet stays visible, as a warning, not an error.
const pausedOnly = foldDeliveryFlaps(conversationEntries([timeline[8]], names));
assert.equal(pausedOnly.length, 1);
assert.equal(pausedOnly[0].body.tone, 'warning');

// Harness markup becomes what it means.
assert.deepEqual(fromHarness(true, '<command-name>/review</command-name><command-args>42</command-args>'), [{ kind: 'user', text: '/review 42' }]);
assert.deepEqual(fromHarness(false, '<task-notification><status>completed</status><summary>Agent finished</summary></task-notification>'), [{ kind: 'event', tone: 'quiet', text: 'background task completed: Agent finished' }]);
assert.equal(fromHarness(true, '<channel source="plugin:st3-channel:st3" from="agent/x">\nSubject: Hi\n</channel>')[0].text, 'delivered to the agent: Hi · from agent/x');
assert.equal(cleanMessageText('[PING] ? hello [id:message/xyz]'), 'hello');
assert.equal(cleanMessageText('keep\n```\n<thinking>code stays</thinking>\n```\n<thinking>gone</thinking>'), 'keep\n```\n<thinking>code stays</thinking>\n```');

// A tool result with no call still shows; a malformed argument still gets a title.
assert.equal(conversationEntries([e('tool_result', 'tool', { call_id: 'nope', status: 'error', content: 'bad' })], names)[0].body.state, 'failed');
assert.equal(toolTitle('Read', '{"file_path":"/a/b"}'), 'Read /a/b');
assert.equal(toolTitle('Weird', 42), 'Weird');
// A body that is not an object at all does not break the conversation.
assert.equal(conversationEntries([e('content', 'assistant', null), e('content', 'assistant', 'plain string')], names).map(describe).join(), 'assistant: plain string');

// Long tool output collapses to its last five lines unless open or failed.
const tool = { kind: 'tool', title: 't', state: 'ok', output: Array.from({ length: 20 }, (_, n) => `line ${n}`) };
assert.equal(shownToolLines(tool, false).hidden, 15);
assert.equal(shownToolLines(tool, false).lines.at(-1), 'line 19');
assert.equal(shownToolLines(tool, true).lines.length, 20);
assert.equal(shownToolLines({ ...tool, state: 'failed' }, false).hidden, 0);

// Half a conversation is not shown: st's notice that it could not read the transcript becomes
// one reason, with the transcript's path.
{
  const notice = details => ({ id: 'n', role: 'system', timestamp: '2026-10-01T10:00:01Z', type: 'error', body: { code: 'transcript-not-bound', message: 'transcript not bound: the transcript could not be read: line 12: expected value', retryable: true, details } });
  const mail = { id: 'm', role: 'user', timestamp: '2026-10-01T10:00:00Z', type: 'content', body: { media_type: 'text/plain', text: 'How is the audit going?' } };
  assert.equal(unreadableTranscript([mail]), null);
  assert.equal(unreadableTranscript([mail, notice({ driver: 'omp', transcript: '/srv/example/omp/sessions/harbor/0190.jsonl' })]), 'This conversation could not be loaded: the transcript could not be read: line 12: expected value (transcript /srv/example/omp/sessions/harbor/0190.jsonl)');
  assert.equal(unreadableTranscript([mail, notice({ driver: 'omp' })]), 'This conversation could not be loaded: the transcript could not be read: line 12: expected value');
}

// A seat that has said nothing since it started shows its Small Talk and one quiet line.
{
  const notYet = { id: 'n', role: 'system', timestamp: '2026-10-01T10:00:01Z', type: 'error', body: { code: 'transcript-not-bound', message: 'transcript not bound: Claude session 0190 has no transcript file yet', retryable: true, details: { driver: 'claude', not_yet: true } } };
  assert.equal(unreadableTranscript([notYet]), null);
  const shown = conversationEntries([notYet], new Map());
  assert.deepEqual(shown.map(entry => entry.body), [{ kind: 'event', tone: 'quiet', text: 'nothing in the harness yet since this seat started' }]);
}

