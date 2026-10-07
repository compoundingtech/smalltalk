import assert from 'node:assert/strict';
import { unreadableTranscript, cleanMessageText, conversationEntries, entryMatches, foldDeliveryFlaps, fromHarness, shownToolLines, toolTitle } from '@smalltalk/st3-views/conversationView';

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
assert.equal(shownToolLines(tool, false).hidden, 14);
assert.equal(shownToolLines(tool, false).lines.at(-1), 'line 19');
assert.equal(shownToolLines(tool, true).lines.length, 20);
assert.equal(shownToolLines({ ...tool, state: 'failed' }, false).hidden, 14, 'failed calls fold too, as in stui');

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


// The person's mail says when the agent has it; the delivery is not another line.
{
  const delivered = conversationEntries([
    { id: 'm', sequence: 1, revision: 1, timestamp: '2026-10-01T10:00:00Z', role: 'user', type: 'message', final: true, body: { message_id: 'message/one', from: 'person/avery', to: 'agent/example/harbor/keeper' } },
    { id: 'c', sequence: 2, revision: 1, timestamp: '2026-10-01T10:00:00Z', role: 'user', type: 'content', final: true, body: { media_type: 'text/plain', text: 'How is the audit going?' } },
    { id: 'h', sequence: 3, revision: 1, timestamp: '2026-10-01T10:00:02Z', role: 'user', type: 'content', final: true, body: { media_type: 'text/plain', text: '<channel source="plugin:st3-channel:st3" from="person/avery">[st3-delivery:1.md]\n[PING from st3] message/one from person/avery: (no subject)\n</channel>' } },
  ], new Map([['person/avery', 'you']]));
  assert.deepEqual(delivered.map(entry => [entry.body.kind, entry.body.delivered]), [['mail', true]]);
}

// A message spoken and transcribed carries st's `dictated` tag; the app marks it.
{
  const spoken = (tags) => conversationEntries([
    { id: 'm', sequence: 1, revision: 1, timestamp: '2026-10-01T10:00:00Z', role: 'user', type: 'message', final: true, body: { message_id: 'message/one', from: 'person/avery', to: 'agent/example/harbor/keeper', ...(tags ? { tags } : {}) } },
    { id: 'c', sequence: 2, revision: 1, timestamp: '2026-10-01T10:00:00Z', role: 'user', type: 'content', final: true, body: { media_type: 'text/plain', text: 'ship the harbor fix' } },
  ], new Map([['person/avery', 'you']]))[0].body;
  assert.equal(spoken(['dictated']).dictated, true);
  assert.equal(spoken(undefined).dictated, undefined);
  assert.equal(spoken(['urgent']).dictated, undefined);
}

// A message a person wrote says which device signed it and whether that checks; an old, unsigned
// one says nothing (Nathan, 2026-10-06).
{
  const signed = (provenance) => conversationEntries([
    { id: 'm', sequence: 1, revision: 1, timestamp: '2026-10-01T10:00:00Z', role: 'user', type: 'message', final: true, body: { message_id: 'message/one', from: 'person/avery', to: 'agent/example/harbor/keeper', ...(provenance ? { provenance } : {}) } },
    { id: 'c', sequence: 2, revision: 1, timestamp: '2026-10-01T10:00:00Z', role: 'user', type: 'content', final: true, body: { media_type: 'text/plain', text: 'ship the harbor fix' } },
  ], new Map([['person/avery', 'you']]))[0].body.signed;
  assert.equal(signed({ verdict: 'verified', signer: 'person/avery', device: 'example phone (secure enclave)', key: 'p256:AAAA' }), '✓ example phone (secure enclave)');
  assert.equal(signed({ verdict: 'verified', signer: 'person/avery' }), '✓ person/avery');
  assert.equal(signed({ verdict: 'held', reason: 'delegation d1 has not arrived' }), '⚠ signature held: delegation d1 has not arrived');
  assert.equal(signed({ verdict: 'invalid', reason: 'the signature does not match the claim' }), '✕ signature invalid: the signature does not match the claim');
  assert.equal(signed({ verdict: 'unsigned' }), undefined);
  assert.equal(signed(undefined), undefined);
}

// A message's images ride on its mail entry; a message may be only its images.
{
  const sent = (text) => conversationEntries([
    { id: 'm', sequence: 1, revision: 1, timestamp: '2026-10-01T10:00:00Z', role: 'user', type: 'message', final: true, body: { message_id: 'message/one', from: 'person/avery', to: 'agent/example/harbor/keeper', attachments: [
      { blob: 'blob/aa', sha256: 'aa', media_type: 'image/png', name: 'Screenshot.png', size: 1200, origin: 'host/example' },
      { blob: 'blob/bb', sha256: 'bb', media_type: 'application/pdf', size: 10, origin: 'host/example' },
    ] } },
    { id: 'c', sequence: 2, revision: 1, timestamp: '2026-10-01T10:00:00Z', role: 'user', type: 'content', final: true, body: { media_type: 'text/plain', text } },
  ], new Map([['person/avery', 'you']]))[0].body;
  assert.deepEqual(sent('see this').images, [{ sha256: 'aa', message: 'message/one', mediaType: 'image/png', name: 'Screenshot.png', size: 1200 }]);
  assert.equal(sent('').text, '', 'an image alone is not a "(notification)"');
}

// Finding: an entry matches by what a person reads in it, case aside.
{
  const mail = { id: 'm', at: '', timestamp: '', body: { kind: 'mail', from: 'Keeper', to: 'you', subject: 'Audit', text: 'The Harbor keys rotated.' } };
  const tool = { id: 't', at: '', timestamp: '', body: { kind: 'tool', title: '$ ls', state: 'ok', output: ['README.md', 'Cargo.toml'] } };
  assert.equal(entryMatches(mail, 'harbor'), true);
  assert.equal(entryMatches(tool, 'cargo'), true);
  assert.equal(entryMatches(tool, 'harbor'), false);
  assert.equal(entryMatches(tool, '  '), true, 'an empty query keeps everything');
}

// "Older entries are not shown" heads the conversation, though st stamps it with the time it was
// read, later than every entry it shows.
{
  const at = (seconds) => `2026-10-02T09:00:${String(seconds).padStart(2, '0')}Z`;
  const timeline = [
    { id: 'e1', sequence: 10, revision: 1, timestamp: at(1), role: 'assistant', final: true, type: 'content', body: { media_type: 'text/plain', text: 'first' } },
    { id: 'e2', sequence: 11, revision: 1, timestamp: at(2), role: 'assistant', final: true, type: 'content', body: { media_type: 'text/plain', text: 'second' } },
    { id: 'cut', sequence: 0, revision: 1, timestamp: at(30), role: 'system', final: true, type: 'truncation', body: { from_sequence: 0, to_sequence: 9, reason: 'outside the window' } },
  ];
  const shown = conversationEntries(timeline, new Map());
  assert.equal(shown[0].body.text, 'older entries are not shown');
  assert.equal(shown[0].at, '');
  assert.deepEqual(shown.slice(1).map(entry => entry.body.text), ['first', 'second']);
}

// A conversation st is refusing says why, how old what is shown is, and that the phone retries.
{
  const { staleLine } = await import('@smalltalk/st3-views/conversationView');
  assert.equal(staleLine('the list changed while it was being read', false, null, 0), 'Not loaded yet: the list changed while it was being read. Trying again.');
  assert.equal(staleLine('willow cannot be reached right now', true, 1_000, 76_000), 'willow cannot be reached right now · shown as of 1m ago · trying again');
  assert.equal(staleLine('st asked to slow down for a moment', true, 10_000, 22_000), 'st asked to slow down for a moment · shown as of 12s ago · trying again');
}

// A compacted conversation's summary is one folded tool-like line, never a giant message with tags.
{
  const [summary] = conversationEntries([
    { id: 's', sequence: 1, revision: 1, timestamp: '2026-10-02T20:50:22Z', role: 'user', type: 'content', final: true, body: { media_type: 'text/plain', text: '<artifact-content-authored-by-others/>\nThis session is being continued from a previous conversation that ran out of context.\n\nSummary:\n1. plan the week' } },
  ], new Map());
  assert.equal(summary.body.kind, 'tool');
  assert.equal(summary.body.title, 'context summary · the conversation was compacted');
  assert.ok(!summary.body.output.some(line => line.includes('<artifact')));
}

// A channel delivery carrying st's envelope is that mail: marked delivered when the stream shows
// it, the mail itself when not, never "delivered to the agent: The person reads replies…"
// (Nathan, 2026-10-03).
{
  const delivery = '<channel source="plugin:st3-channel:st3" from="person/example" messageId="message/c3">\n<smalltalk-message id="c3" from="person/example" to="agent/example/quay" subject="(no subject)" sha256="00" graph="message/c3">\nhow is it &lt;going&gt;?\n</smalltalk-message>\nThe person reads replies in st, not in the agent\'s session.\n</channel>';
  const delivered = new Set();
  assert.deepEqual(fromHarness(true, delivery, new Set(['message/c3']), delivered), []);
  assert.ok(delivered.has('message/c3'));
  const [mail, ...rest] = fromHarness(true, delivery);
  assert.equal(rest.length, 0);
  assert.deepEqual([mail.kind, mail.from, mail.text], ['mail', 'person/example', 'how is it <going>?']);
}

// A Codex seat's turn carries st's envelope and its delivery notes: the mail, never raw XML or
// words the person typed (Nathan, 2026-10-03).
{
  const turn = "(dictated by voice; it may contain transcription mistakes)\n<smalltalk-message id=\"e5\" from=\"person/example\" to=\"agent/example/quay\" subject=\"(no subject)\" sha256=\"00\" graph=\"message/e5\">\nis this a watcher?\n</smalltalk-message>\nThe person reads replies in st, not in the agent's session.";
  const delivered = new Set();
  assert.deepEqual(fromHarness(true, turn, new Set(['message/e5']), delivered), []);
  assert.ok(delivered.has('message/e5'));
  const bodies = fromHarness(true, turn);
  assert.deepEqual(bodies.map(body => [body.kind, body.text]), [['mail', 'is this a watcher?']]);
}
