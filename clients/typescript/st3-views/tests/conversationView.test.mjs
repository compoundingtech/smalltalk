import assert from 'node:assert/strict';
import { unreadableTranscript, cleanMessageText, conversationEntries, fetchedConversationEntries, entryMatches, foldDeliveryFlaps, fromHarness, headerLine, shownToolLines, subagentSession, toolTitle, DEFAULT_FILTERS, SHOW_EVERYTHING } from '@smalltalk/st3-views/conversationView';

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

// A source-window omission distinguishes an unavailable remainder from an owner chunk.
{
  const notice = e('truncation', 'system', { reason: 'native transcript prefix; not fetchable through this owner read', omitted_from_sequence: 0, omitted_to_sequence: 0 });
  assert.match(conversationEntries([notice], names)[0].body.text, /not fetchable/);
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

const exposed = '<analysis>invented-token</analysis><thinking>visible</thinking>';
assert.equal(JSON.parse(conversationEntries([{id:'raw',timestamp:'2026-10-06T12:00:00Z',role:'assistant',type:'content',body:{text:exposed}}], new Map(), SHOW_EVERYTHING)[0].body.text).body.text, exposed);

const unknownNative = '[unrecognized future]\n{"raw":{"token":"invented-token"}}';
assert.equal(conversationEntries([{id:'unknown',timestamp:'2026-10-06T12:00:00Z',role:'system',type:'content',body:{text:unknownNative,blocks:[{kind:'unknown'}]}}],new Map())[0].body.text,unknownNative);
assert.equal(cleanMessageText('<analysis>visible invented-token</analysis>', SHOW_EVERYTHING), '<analysis>visible invented-token</analysis>');
assert.deepEqual(fromHarness(true,'<analysis>visible invented-token</analysis>'), []);

// Display preferences do not mutate the normalized data; raw mode is reversible JSON.
const rawText = '<system-reminder>invented-token</system-reminder>visible\u001b\n<thinking>private reasoning</thinking>';
const rawEntry = e('content', 'user', {text:rawText, blocks:[{id:'source', kind:'source_record', source_type:'claude', visibility:'internal', payload:{raw:{text:rawText}}}]});
const original = structuredClone(rawEntry);
assert.equal(conversationEntries([rawEntry], names, DEFAULT_FILTERS)[0].body.text, 'visible');
assert.deepEqual(JSON.parse(conversationEntries([rawEntry], names, SHOW_EVERYTHING)[0].body.text), original);
assert.deepEqual(rawEntry, original);
assert.equal(cleanMessageText(rawText, SHOW_EVERYTHING), rawText);

// Blocks that expose native data (#1574) are data: the markup cleanup must not eat reasoning
// text or raw JSON, while plain prose without them is still cleaned, as stui does.
{
  const base = { id: 'r', timestamp: '2026-10-06T12:00:00Z', role: 'assistant', type: 'content', final: true, sequence: 1 };
  const reasoning = conversationEntries([{ ...base, body: { media_type: 'text/plain', text: '<thinking>the plan: check the fixtures</thinking>', blocks: [{ id: 'b', kind: 'reasoning', source_type: 'synthetic', payload: {} }] } }], new Map());
  assert.equal(reasoning[0].body.text, '<thinking>the plan: check the fixtures</thinking>', 'reasoning text arrives as data');
  const prose = conversationEntries([{ ...base, body: { media_type: 'text/plain', text: '<thinking>private</thinking>visible' } }], new Map());
  assert.equal(prose[0].body.text, 'visible', 'prose without data blocks is still cleaned');
  const raw = conversationEntries([{ ...base, role: 'system', body: { media_type: 'text/plain', text: '<tool_result>{"output":1}</tool_result>', blocks: [{ id: 'b', kind: 'unknown', source_type: 'synthetic', payload: {} }] } }], new Map());
  assert.equal(raw[0].body.text, '<tool_result>{"output":1}</tool_result>', 'unknown raw JSON arrives as data');
}

// A subagent card's `open session/…` line is the link to that conversation (q2).
{
  assert.equal(subagentSession(['Review synthetic code', 'duration 1200ms', 'open session/child']), 'session/child');
  assert.equal(subagentSession(['open session/child', 'open session/other']), 'session/child', 'the first link wins');
  assert.equal(subagentSession(['reviewed, nothing to open']), undefined);
}

// Hand-built mixed-source header: only the register field gets an individual marker.
{
  const asOf = '2026-10-06T12:00:00Z';
  const field = (value, source = 'transcript') => ({ value, source, as_of: asOf });
  assert.equal(headerLine({
    model: field('synthetic/model'),
    context: field({ tokens: 50, window: null }),
    cost: field({ usd: 0.02 }),
    todos: field([{ phase: 'Render', items: [{ content: 'Render cards', status: 'completed' }] }]),
    jobs: field([{ id: 'job-2', state: 'running' }]),
    subagents: field([{ id: 'child-live', status: 'running' }]),
    ask: field({ call_id: 'active', questions: [{ question: 'Continue?', options: [], multi: false }] }),
    working: field(true, 'register'),
  }, asOf), 'model synthetic/model · context 50 tokens · cost $0.02 · todo 1/1 · jobs 1 · agents 1 · ask Continue? · working [register · 0s ago] · transcript · 0s ago');
  assert.equal(headerLine({ cost: { value: { usd: 3 }, source: 'register', as_of: '2026-10-06T11:30:00Z' } }, asOf), 'cost $3.00 · register · 30m ago');
  assert.equal(headerLine({ model: field('synthetic/model ') }, '2026-10-06T12:59:30Z'), 'model synthetic/model · transcript · 1h ago', 'rounded minutes promote to hours without extra spaces');
  assert.equal(headerLine({ working: field(false, 'register') }, '2026-10-07T11:30:00Z'), 'idle · register · 1d ago', 'rounded hours promote to days');
  assert.equal(headerLine({ working: field(false, 'register') }, asOf), 'idle · register · 0s ago');
  assert.equal(headerLine({ context: field({ tokens: 42, window: 100 }, 'register') }, asOf), 'context 42 tokens of 100 · register · 0s ago');
  assert.equal(headerLine({ context: field({ tokens: null, window: 100 }, 'register') }, asOf), 'context limit 100 tokens · register · 0s ago');
  assert.equal(headerLine({ ask: field(null) }, asOf), null, 'a header with nothing to say says nothing');
  assert.equal(headerLine(undefined, asOf), null);
}

// Shared provenance conservatively ages from its oldest field, even with a live register cost.
{
  const header = {
    model: { value: 'm', source: 'transcript', as_of: '2026-10-06T11:59:00Z' },
    context: { value: { tokens: 50 }, source: 'transcript', as_of: '2026-10-06T11:00:00Z' },
    cost: { value: { usd: 0.02 }, source: 'register', as_of: '2026-10-06T11:30:00Z' },
  };
  assert.equal(headerLine(header, '2026-10-06T12:00:00Z'), 'model m · context 50 tokens · cost $0.02 [register · 30m ago] · transcript · 1h ago');
  assert.equal(headerLine({ todos: { value: [], source: 'transcript', as_of: '2026-10-06T12:00:00Z' } }, '2026-10-06T12:00:00Z'), 'todo 0/0 · transcript · 0s ago');
}

// Parity gaps: expanded calls retain invocation rows after the receipt arrives.
{
  const block = (kind, view, extra = {}) => ({ id: 'synthetic-block', kind, source_type: 'synthetic', payload: {}, view, ...extra });
  const cases = [
    [{ type: 'bash', command: 'echo synthetic', cwd: '/synthetic', timeout_s: 30 }, ['cwd: /synthetic', 'timeout: 30s']],
    [{ type: 'write', path: 'demo', content: 'first\nlast', line_count: 2, bytes: 10 }, ['first', 'last']],
    [{ type: 'eval', language: 'py', code: '1 + 2\n3 + 4', timeout_s: 5, reset: false }, ['language: py', 'timeout: 5s', 'reset: false', '1 + 2', '3 + 4']],
    [{ type: 'hub', op: 'send', target: 'Child', message: 'sent\nbody' }, ['sent', 'body']],
    [{ type: 'search', engine: 'grep', pattern: 'needle', case: false, gitignore: true, skip: 2 }, ['case: false', 'gitignore: true', 'skip: 2']],
  ];
  for (const [view, expected] of cases) {
    const call = e('tool_call', 'assistant', { call_id: view.type, name: view.type, arguments: {}, blocks: [block('tool_call', view)] });
    const result = e('tool_result', 'tool', { call_id: view.type, status: 'success', content: 'receipt' });
    const [entry] = conversationEntries([call, result], names);
    for (const line of expected) assert(entry.body.output.includes(line), `${view.type}: ${line}`);
    assert.equal(entry.body.output.at(-1), 'receipt');
  }
  const grep = e('tool_result', 'tool', { call_id: 'g', status: 'success', content: 'native matches', blocks: [block('tool_output', { type: 'search', match_count: 7, file_count: 3, truncated: true, file_limit_reached: 3, per_file_limit_reached: 2 })] });
  assert.deepEqual(conversationEntries([grep], names)[0].body.output, ['7 matches / 3 files', 'warning: search results truncated', 'warning: file limit reached (3)', 'warning: per-file limit reached (2)', 'native matches']);
  const compaction = e('content', 'system', { media_type: 'text/plain', blocks: [block('status', { type: 'compaction', summary: Array.from({ length: 10 }, (_, i) => `summary-${i}`).join('\n') })] });
  const [summary] = conversationEntries([compaction], names);
  assert.deepEqual(shownToolLines(summary.body, false).lines, ['summary-0', 'summary-1', 'summary-2', 'summary-3', 'summary-4', 'summary-5']);
  assert.equal(shownToolLines(summary.body, true).lines.at(-1), 'summary-9');
  const error = e('content', 'assistant', { blocks: [block('error', { type: 'assistant_error', status: 'recovered', presentation: 'compact-recovered', is_error: false, message: 'synthetic error', retry: { note: 'retried successfully' } })] });
  assert.deepEqual(conversationEntries([error], names)[0].body.output, ['synthetic error', 'retried successfully']);
  const hiddenError = e('content', 'assistant', { blocks: [block('error', { type: 'assistant_error', presentation: 'none' })] });
  assert.equal(conversationEntries([hiddenError], names).length, 0);
  const reference = { ref: 'full-write', media_type: 'application/json', reason: 'size-limit' };
  const view = { type: 'write', path: 'demo', content: 'first\n[st truncated this native timeline value: size limit; 20000 bytes]', line_count: 2, bytes: 20000 };
  const call = e('tool_call', 'assistant', { call_id: 'large-write', name: 'write', arguments: {}, blocks: [block('tool_call', view, { continuation: reference })] });
  const [entry] = conversationEntries([call], names);
  const hydrated = fetchedConversationEntries(entry, reference, { ...view, content: 'first\nlast' });
  assert.deepEqual(hydrated[0].body.output, ['first', 'last'], 'view-only continuations hydrate written content through the typed adapter');
}
