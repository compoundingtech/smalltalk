const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const ts = require('typescript');

const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'st3-ts-client-'));
for (const name of ['Models.generated', 'Client.generated', 'fetch-receiver.test']) {
    const source = fs.readFileSync(path.join(__dirname, `${name}.ts`), 'utf8');
    const output = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020, rewriteRelativeImportExtensions: true } }).outputText;
    fs.writeFileSync(path.join(temporary, `${name}.js`), output);
}
const { St3Client, ClientError, applyWindow } = require(path.join(temporary, 'Client.generated.js'));
require(path.join(temporary, 'fetch-receiver.test.js'));
fs.rmSync(temporary, { recursive: true, force: true });

const snapshot = { id: 'snapshot/test', host_id: 'host/test', store_index: 1, projection_version: 'client-projection.v0', created_at: '2026-09-20T00:00:00Z' };
function envelope(value) { return { api_version: 'st3.client.v0', request_id: 'request/test', snapshot, value }; }
function response(value, status = 200) { return { ok: status < 400, status, json: async () => value }; }
const capabilityFixture = require('../../../docs/st3/client-v0/fixtures/capabilities.json');
const cursorGapFixture = require('../../../docs/st3/client-v0/fixtures/cursor-gap-error.json');
const capabilities = { ...capabilityFixture.value, limits: { ...capabilityFixture.value.limits, max_page_items: 2, max_event_items: 3, max_wait_ms: 10 } };

const validTraceparent = '00-0123456789abcdef0123456789abcdef-0123456789abcdef-01';
const traceCases = [
    ['absent callback', undefined, undefined],
    ['undefined context', () => undefined, undefined],
    ['valid context', () => ({ traceparent: validTraceparent }), { traceparent: validTraceparent }],
    ['verbatim tracestate', () => ({ traceparent: validTraceparent, tracestate: 'vendor=value,other=opaque' }), { traceparent: validTraceparent, tracestate: 'vendor=value,other=opaque' }],
    ['empty tracestate', () => ({ traceparent: validTraceparent, tracestate: '' }), { traceparent: validTraceparent, tracestate: '' }],
    ...[
        'not-a-traceparent',
        validTraceparent.toUpperCase(),
        validTraceparent + '\n',
        'ff' + validTraceparent.slice(2),
        '00-00000000000000000000000000000000-0123456789abcdef-01',
        '00-0123456789abcdef0123456789abcdef-0000000000000000-01',
    ].map(parent => [`invalid ${JSON.stringify(parent)}`, () => ({ traceparent: parent, tracestate: 'vendor=must-not-leak' }), undefined]),
];

for (const [name, callback, expected] of traceCases) {
    test(`trace context: ${name} on HTTP and every WebSocket open`, async () => {
        let reads = 0;
        const headers = [];
        const client = new St3Client({
            baseUrl: 'https://example.test',
            ...(callback === undefined ? {} : { traceContext: () => { reads++; return callback(); } }),
            fetchImpl: async (_url, init) => { headers.push(init.headers); return response(envelope(capabilities)); },
        });
        const socket = (_url, _protocols, fields) => {
            headers.push(fields);
            return { onopen: null, onmessage: null, onclose: null, onerror: null, send() {}, close() {} };
        };
        await client.capabilities();
        const terminal = await client.terminalStream('terminal/test', { streamCapability: 'proof', onScreen() {}, socket });
        const conversation = await client.conversationStream('session/test', { onChange() {}, socket });
        const collection = await client.collectionStream({ onFrame() {}, socket });
        terminal.close();
        conversation.close();
        collection.close();
        assert.equal(headers.length, 4);
        assert.equal(reads, callback === undefined ? 0 : 4);
        for (const fields of headers) {
            assert.equal(fields.traceparent, expected?.traceparent);
            assert.equal(fields.tracestate, expected?.tracestate);
            assert.equal(Object.hasOwn(fields, 'traceparent'), expected !== undefined);
            assert.equal(Object.hasOwn(fields, 'tracestate'), expected?.tracestate !== undefined);
        }
    });
}

test('trace context is read afresh for each HTTP request', async () => {
    let active;
    const headers = [];
    const client = new St3Client({ baseUrl: 'https://example.test', traceContext: () => active,
        fetchImpl: async (_url, init) => { headers.push(init.headers); return response(envelope(capabilities)); } });
    await client.capabilities();
    active = { traceparent: validTraceparent };
    await client.capabilities();
    active = undefined;
    await client.capabilities();
    assert.deepEqual(headers.map(fields => fields.traceparent), [undefined, validTraceparent, undefined]);
});

test('discovers capabilities, bounds pages, and encodes opaque cursors', async () => {
    const calls = [];
    const client = new St3Client({ baseUrl: 'https://example.test/', credential: () => 'secret', fetchImpl: async (url, init) => {
        calls.push({ url, init });
        return response(envelope(url.endsWith('/capabilities') ? capabilities : { kind: 'page', collection: 'missions', filters: {}, items: [], page: { limit: 2, has_more: false } }));
    } });
    await client.missionsList({ cursor: 'abc+/=', limit: 2 });
    assert.equal(calls.length, 2);
    assert.equal(calls[1].url, 'https://example.test/v1/client/missions?cursor=abc%2B%2F%3D&limit=2');
    assert.equal(calls[1].init.headers.Authorization, 'Bearer secret');
    await assert.rejects(client.missionsList({ limit: 3 }), RangeError);
    assert.equal(calls.length, 2);
});

test('sends typed fenced action and follows the returned operation', async () => {
    const calls = [];
    const client = new St3Client({ baseUrl: 'https://example.test', fetchImpl: async (url, init) => {
        calls.push({ url, init });
        if (url.endsWith('/capabilities')) return response(envelope(capabilities));
        if (url.endsWith('/actions')) return response(envelope({ kind: 'action-result', action_id: 'action/test', operation_id: 'operation/test', status: 'accepted', affected_ids: [], snapshot_id: snapshot.id }), 202);
        return response(envelope({ kind: 'operation', id: 'operation/test', revision: '1', updated_at: '2026-09-20T00:00:00Z', component: 'action', severity: 'info', state: 'completed', summary: 'done' }));
    } });
    const result = await client.messageSend({ id: 'action/test', idempotency_key: 'test-idempotency-key', fence: { snapshot_id: snapshot.id, subject_revisions: {} }, parameters: { to: 'agent/test', content: 'hello' } });
    const body = JSON.parse(calls[1].init.body);
    assert.equal(body.type, 'message.send');
    assert.equal(body.fence.snapshot_id, snapshot.id);
    assert.equal(body.actor, undefined);
    const operation = await client.followOperation(result.value.operation_id);
    assert.equal(operation.value.state, 'completed');
    assert.match(calls[2].url, /\/operations\/operation%2Ftest$/);
});

test('preserves versioned cursor gap errors', async () => {
    const client = new St3Client({ baseUrl: 'https://example.test', fetchImpl: async (url) => {
        if (url.endsWith('/capabilities')) return response(envelope(capabilities));
        return response(cursorGapFixture, 409);
    } });
    await assert.rejects(client.eventsList({ after: 'cursor/old', limit: 3 }), error => {
        assert.ok(error instanceof ClientError);
        assert.equal(error.response.code, 'cursor-gap');
        return true;
    });
});

test('completes pairing with either canonical or bare pairing ID', async () => {
    const calls = [];
    const client = new St3Client({ baseUrl: 'https://example.test', fetchImpl: async (url, init) => {
        calls.push({ url, init });
        return response(envelope({ kind: 'paired-session', device_id: 'device/test', person_id: 'person/test', session_actor: 'person/test/session/test', credential: 'proof', scopes: [], expires_at: '2026-10-01T00:00:00Z' }));
    } });
    for (const id of ['pairing/test', 'test']) {
        await client.completePairing(id, { api_version: 'st3.client.v0', code: 'ABCDEFGH', device_public_key: 'public-key-for-test-device-00000000' });
    }
    assert.equal(calls.length, 2);
    assert.ok(calls.every(call => call.url === 'https://example.test/v1/client/pairings/test/complete'));
    assert.ok(calls.every(call => call.init.method === 'POST'));
});

test('follows terminal screens until the server ends the stream with its error', async () => {
    const screenFixture = require('../../../docs/st3/client-v0/fixtures/terminal-screen.json');
    const changedFixture = require('../../../docs/st3/client-v0/fixtures/terminal-screen-changed.json');
    const staleFixture = require('../../../docs/st3/client-v0/fixtures/terminal-stale-fence-error.json');
    const opened = [];
    const socket = { onmessage: null, onclose: null, onerror: null, closed: [], close(code) { this.closed.push(code); } };
    const client = new St3Client({ baseUrl: 'https://example.test/', credential: () => 'secret', fetchImpl: async () => { throw new Error('no HTTP'); } });
    const screens = [];
    let ended;
    await client.terminalStream('terminal/release-shell', {
        streamCapability: 'capability-proof',
        incarnation: 'pty-4:2026-09-20T11:10:00Z',
        onScreen: screen => screens.push(screen),
        onEnd: error => { ended = error ?? null; },
        socket: (url, protocols, headers) => { opened.push({ url, protocols, headers }); return socket; },
    });
    assert.deepEqual(opened, [{
        url: 'wss://example.test/v1/client/terminals/terminal%2Frelease-shell/stream?incarnation=pty-4%3A2026-09-20T11%3A10%3A00Z',
        protocols: ['st3.client.terminal.v0', 'st3.cap.capability-proof'],
        headers: { Authorization: 'Bearer secret', 'x-st3-features': 'custom-subjects.v1, conversation-blocks.v1' },
    }]);
    socket.onmessage({ data: JSON.stringify(screenFixture) });
    socket.onmessage({ data: JSON.stringify(changedFixture) });
    assert.deepEqual(screens.map(screen => screen.value.revision), [screenFixture.value.revision, changedFixture.value.revision]);
    assert.equal(ended, undefined);
    socket.onmessage({ data: JSON.stringify(staleFixture) });
    assert.ok(ended instanceof ClientError);
    assert.equal(ended.response.code, 'stale-fence');
    assert.deepEqual(socket.closed, [1000]);
    assert.equal(socket.onmessage, null);
});

test('a normal close ends the terminal stream without an error', async () => {
    const socket = { onmessage: null, onclose: null, onerror: null, close() {} };
    const client = new St3Client({ baseUrl: 'http://100.64.0.1:7777', fetchImpl: async () => { throw new Error('no HTTP'); } });
    const ends = [];
    const stream = await client.terminalStream('terminal/release-shell', { streamCapability: 'c', onScreen: () => {}, onEnd: error => ends.push(error), socket: url => { assert.ok(url.startsWith('ws://100.64.0.1:7777/')); return socket; } });
    socket.onclose({ code: 1000, reason: '' });
    stream.close();
    assert.deepEqual(ends, [undefined]);
});

test('conversation stream opens at a cursor and delivers bounded changes', async () => {
    const socket = { onmessage: null, onclose: null, onerror: null, close() {} };
    const opened = [];
    const received = [];
    const client = new St3Client({ baseUrl: 'https://example.test', credential: () => 'secret', fetchImpl: async () => { throw new Error('no HTTP'); } });
    const stream = await client.conversationStream('session/example', {
        after: 'conversation-cursor/owner/example/1.2.3',
        onChange: change => received.push(change.value),
        socket: (url, protocols, headers) => { opened.push({ url, protocols, headers }); return socket; },
    });
    assert.deepEqual(opened, [{ url: 'wss://example.test/v1/client/conversations/example/stream?after=conversation-cursor%2Fowner%2Fexample%2F1.2.3', protocols: ['st3.client.conversation.v0'], headers: { Authorization: 'Bearer secret', 'x-st3-features': 'custom-subjects.v1, conversation-blocks.v1' } }]);
    const change = { kind: 'conversation-changes', session_id: 'session/example', items: [{ id: 'timeline-entry/example', sequence: 4, revision: 1, timestamp: snapshot.created_at, role: 'assistant', type: 'content', final: true, body: { media_type: 'text/plain', text: 'reply' } }], next_cursor: 'conversation-cursor/owner/example/1.3.3' };
    socket.onmessage({ data: JSON.stringify(envelope(change)) });
    assert.deepEqual(received, [change]);
    stream.close();
});

function collectionSocket() {
    return { onopen: null, onmessage: null, onclose: null, onerror: null, sent: [], closed: [], send(text) { this.sent.push(JSON.parse(text)); }, close(code) { this.closed.push(code); } };
}
const mission = (id, title) => ({ kind: 'mission', id, revision: `${id}@1`, updated_at: snapshot.created_at, title, state: 'running', runs: [], run_generations: {}, mission_revision: 'r' });

test('collection stream holds commands until the socket opens and passes frames through', async () => {
    const socket = collectionSocket();
    const opened = [];
    const lifecycle = [];
    const frames = [];
    const ends = [];
    const client = new St3Client({ baseUrl: 'https://example.test/', credential: () => 'secret', fetchImpl: async () => { throw new Error('no HTTP'); } });
    const stream = await client.collectionStream({ onFrame: frame => frames.push(frame), onEnd: error => ends.push(error), onOpen: () => lifecycle.push(['open']), onCommandSent: command => lifecycle.push([command.kind, command.id]), socket: (url, protocols, headers) => { opened.push({ url, protocols, headers }); return socket; } });
    assert.deepEqual(opened, [{ url: 'wss://example.test/v1/client/collections/stream', protocols: ['st3.client.collections.v0'], headers: { Authorization: 'Bearer secret', 'x-st3-features': 'custom-subjects.v1, conversation-blocks.v1' } }]);
    stream.subscribe('missions', 'missions', 200);
    stream.subscribe('mine', 'attention', 50, { person: 'person/example' });
    assert.deepEqual(socket.sent, []);
    assert.deepEqual(lifecycle, []);
    socket.onopen();
    assert.deepEqual(lifecycle, [['open'], ['subscribe', 'missions'], ['subscribe', 'mine']]);
    stream.subscribeTerminal('term', 'terminal/example', 'pty-1:2026-09-20T00:00:00Z', 'capability-proof');
    stream.subscribeConversation('talk', 'agent/example');
    stream.unsubscribe('talk');
    assert.deepEqual(lifecycle.slice(-3), [['subscribe', 'term'], ['subscribe', 'talk'], ['unsubscribe', 'talk']]);
    assert.deepEqual(socket.sent, [
        { kind: 'subscribe', id: 'missions', collection: 'missions', limit: 200 },
        { kind: 'subscribe', id: 'mine', collection: 'attention', limit: 50, person: 'person/example' },
        { kind: 'subscribe', id: 'term', collection: 'terminal', terminal: 'terminal/example', incarnation: 'pty-1:2026-09-20T00:00:00Z', capability: 'capability-proof' },
        { kind: 'subscribe', id: 'talk', collection: 'conversation', conversation: 'agent/example' },
        { kind: 'unsubscribe', id: 'talk' },
    ]);
    // A terminal's stale fence ends that subscription only: the socket and its windows keep going.
    socket.onmessage({ data: JSON.stringify({ kind: 'error', id: 'term', collection: 'terminal', code: 'stale-fence', message: 'the terminal restarted' }) });
    socket.onmessage({ data: JSON.stringify({ kind: 'resync', id: 'missions' }) });
    socket.onmessage({ data: JSON.stringify({ kind: 'snapshot', id: 'missions', collection: 'missions', snapshot, items: [], order: [], has_more: false }) });
    assert.deepEqual(frames.map(frame => [frame.kind, frame.id]), [['error', 'term'], ['resync', 'missions'], ['snapshot', 'missions']]);
    assert.deepEqual(ends, []);
    socket.onmessage({ data: 'not json' });
    assert.equal(ends.length, 1);
    assert.deepEqual(socket.closed, [1000]);
});

test('command observers see queued and direct commands only after socket.send', async () => {
    const socket = collectionSocket();
    const observed = [];
    const ends = [];
    const client = new St3Client({ baseUrl: 'https://example.test', fetchImpl: async () => { throw new Error('no HTTP'); } });
    const stream = await client.collectionStream({
        onFrame: () => {},
        onEnd: error => ends.push(error),
        onCommandSent: command => {
            assert.equal(socket.sent.length, observed.length + 1);
            assert.deepEqual({ kind: socket.sent.at(-1).kind, id: socket.sent.at(-1).id }, command);
            observed.push(command);
        },
        socket: () => socket,
    });
    stream.subscribe('queued', 'agents');
    assert.deepEqual(observed, []);
    socket.onopen();
    stream.subscribe('direct', 'missions');
    stream.unsubscribe('direct');
    assert.deepEqual(observed, [
        { kind: 'subscribe', id: 'queued' },
        { kind: 'subscribe', id: 'direct' },
        { kind: 'unsubscribe', id: 'direct' },
    ]);
    assert.deepEqual(ends, []);
    stream.close();
});

test('a replacement stream opens and reports resubscription sends afresh after an end', async () => {
    const sockets = [collectionSocket(), collectionSocket()];
    const lifecycle = [];
    const ends = [];
    let created = 0;
    const client = new St3Client({ baseUrl: 'https://example.test', fetchImpl: async () => { throw new Error('no HTTP'); } });
    const createStream = index => client.collectionStream({
        onFrame: () => {},
        onEnd: error => ends.push([index, error]),
        onOpen: () => lifecycle.push([index, 'open']),
        onCommandSent: command => {
            const socket = sockets[index];
            const sent = lifecycle.filter(event => event[0] === index && event[1] !== 'open');
            assert.equal(socket.sent.length, sent.length + 1);
            assert.deepEqual({ kind: socket.sent.at(-1).kind, id: socket.sent.at(-1).id }, command);
            lifecycle.push([index, command.kind, command.id]);
        },
        socket: () => { created += 1; return sockets[index]; },
    });
    const first = await createStream(0);
    first.subscribe('agents', 'agents');
    first.subscribe('missions', 'missions');
    sockets[0].onopen();
    sockets[0].onclose({ code: 1000 });
    assert.equal(created, 1); // Reconnection requires a new collectionStream call.
    assert.deepEqual(ends, [[0, undefined]]);
    assert.equal(sockets[0].onopen, null);
    assert.equal(sockets[0].onclose, null);
    const replacement = await createStream(1);
    replacement.subscribe('agents', 'agents');
    replacement.subscribe('missions', 'missions');
    assert.deepEqual(sockets[1].sent, []);
    sockets[1].onopen();
    first.subscribe('ignored', 'agents');
    first.unsubscribe('agents');
    first.close();
    assert.equal(created, 2);
    assert.deepEqual(lifecycle, [
        [0, 'open'], [0, 'subscribe', 'agents'], [0, 'subscribe', 'missions'],
        [1, 'open'], [1, 'subscribe', 'agents'], [1, 'subscribe', 'missions'],
    ]);
    assert.deepEqual(sockets[0].sent.map(command => command.id), ['agents', 'missions']);
    assert.deepEqual(sockets[1].sent.map(command => command.id), ['agents', 'missions']);
    assert.deepEqual(ends, [[0, undefined]]);
    replacement.close();
});

test('closing from onOpen cancels all queued commands', async () => {
    const socket = collectionSocket();
    const lifecycle = [];
    const client = new St3Client({ baseUrl: 'https://example.test', fetchImpl: async () => { throw new Error('no HTTP'); } });
    let stream;
    stream = await client.collectionStream({ onFrame: () => {}, onOpen: () => { lifecycle.push('open'); stream.close(); }, onCommandSent: command => lifecycle.push(command), socket: () => socket });
    stream.subscribe('agents', 'agents');
    stream.subscribe('missions', 'missions');
    socket.onopen();
    assert.deepEqual(socket.sent, []);
    assert.deepEqual(lifecycle, ['open']);
    assert.deepEqual(socket.closed, [1000]);
});

test('an onOpen observer error ends the stream before queued commands are sent', async () => {
    const socket = collectionSocket();
    const ends = [];
    const client = new St3Client({ baseUrl: 'https://example.test', fetchImpl: async () => { throw new Error('no HTTP'); } });
    const observerError = new Error('open observer failed');
    const stream = await client.collectionStream({ onFrame: () => {}, onEnd: error => ends.push(error), onOpen: () => { throw observerError; }, socket: () => socket });
    stream.subscribe('agents', 'agents');
    stream.subscribe('missions', 'missions');
    socket.onopen();
    assert.deepEqual(socket.sent, []);
    assert.deepEqual(ends, [observerError]);
    assert.deepEqual(socket.closed, [1000]);
});

test('an onCommandSent observer error ends the stream after the sent command', async () => {
    const socket = collectionSocket();
    const ends = [];
    const client = new St3Client({ baseUrl: 'https://example.test', fetchImpl: async () => { throw new Error('no HTTP'); } });
    const observerError = new Error('send observer failed');
    const stream = await client.collectionStream({ onFrame: () => {}, onEnd: error => ends.push(error), onCommandSent: command => { if (command.id === 'agents') throw observerError; }, socket: () => socket });
    stream.subscribe('agents', 'agents');
    stream.subscribe('missions', 'missions');
    socket.onopen();
    assert.deepEqual(socket.sent.map(command => command.id), ['agents']);
    assert.deepEqual(ends, [observerError]);
    assert.deepEqual(socket.closed, [1000]);
});

test('closing from the first command observer stops later queued sends', async () => {
    const socket = collectionSocket();
    const sent = [];
    const client = new St3Client({ baseUrl: 'https://example.test', fetchImpl: async () => { throw new Error('no HTTP'); } });
    let stream;
    stream = await client.collectionStream({ onFrame: () => {}, onCommandSent: command => { sent.push(command.id); stream.close(); }, socket: () => socket });
    stream.subscribe('agents', 'agents');
    stream.subscribe('missions', 'missions');
    socket.onopen();
    assert.deepEqual(socket.sent.map(command => command.id), ['agents']);
    assert.deepEqual(sent, ['agents']);
    assert.deepEqual(socket.closed, [1000]);
});

test('collection commands preserve omitted and nullable options and failure metadata', async () => {
    const socket = collectionSocket();
    const frames = [];
    const client = new St3Client({ baseUrl: 'https://example.test', fetchImpl: async () => { throw new Error('no HTTP'); } });
    const stream = await client.collectionStream({ onFrame: frame => frames.push(frame), socket: () => socket });
    socket.onopen();
    stream.subscribe('agents', 'agents');
    stream.subscribe('nullable', 'work', 100, { person: null, actor: null, status: null });
    stream.subscribeTerminal('current', 'terminal/example', undefined, 'capability-proof');
    stream.subscribeTerminal('nullable-terminal', 'terminal/example', null, 'capability-proof');
    assert.deepEqual(socket.sent, [
        { kind: 'subscribe', id: 'agents', collection: 'agents' },
        { kind: 'subscribe', id: 'nullable', collection: 'work', limit: 100, person: null, actor: null, status: null },
        { kind: 'subscribe', id: 'current', collection: 'terminal', terminal: 'terminal/example', capability: 'capability-proof' },
        { kind: 'subscribe', id: 'nullable-terminal', collection: 'terminal', terminal: 'terminal/example', incarnation: null, capability: 'capability-proof' },
    ]);
    const failures = [
        { kind: 'resync', id: 'agents', code: 'internal', message: 'Retry the read', retryable: true },
        { kind: 'resync', id: 'chat', collection: 'conversation', retryable: true },
        { kind: 'error', id: 'chat', collection: 'conversation', code: 'timeline-history-incomplete', message: 'History missing', retryable: false },
        { kind: 'error', id: 'current', collection: 'terminal', code: 'stale-fence', message: 'Restarted', retryable: false },
    ];
    for (const frame of failures) socket.onmessage({ data: JSON.stringify(frame) });
    assert.deepEqual(frames, failures);
    stream.close();
});

test('closing the collection stream sends nothing more and reports no end', async () => {
    const socket = collectionSocket();
    const ends = [];
    const client = new St3Client({ baseUrl: 'http://100.64.0.1:7777', fetchImpl: async () => { throw new Error('no HTTP'); } });
    const stream = await client.collectionStream({ onFrame: () => {}, onEnd: error => ends.push(error), socket: url => { assert.ok(url.startsWith('ws://100.64.0.1:7777/')); return socket; } });
    socket.onopen();
    stream.close();
    stream.subscribe('agents', 'agents');
    assert.deepEqual(socket.sent, []);
    assert.deepEqual(socket.closed, [1000]);
    assert.deepEqual(ends, []);
});

test('applyWindow keeps a window in the order each frame names', () => {
    const [a, b, c] = [mission('mission/a', 'A'), mission('mission/b', 'B'), mission('mission/c', 'C')];
    let window = applyWindow(undefined, { kind: 'changes', id: 'm', collection: 'missions', snapshot, upserts: [a], removes: [], order: ['mission/a'], has_more: false });
    assert.equal(window, undefined);
    window = applyWindow(window, { kind: 'snapshot', id: 'm', collection: 'missions', snapshot, items: [b, a], order: ['mission/a', 'mission/b'], has_more: true });
    assert.deepEqual(window.items.map(item => item.id), ['mission/a', 'mission/b']);
    assert.equal(window.hasMore, true);
    const renamed = { ...b, title: 'B again' };
    const later = { ...snapshot, id: 'snapshot/later', store_index: 2 };
    window = applyWindow(window, { kind: 'changes', id: 'm', collection: 'missions', snapshot: later, upserts: [c, renamed], removes: ['mission/a'], order: ['mission/b', 'mission/c'], has_more: false });
    assert.deepEqual(window.items.map(item => [item.id, item.title]), [['mission/b', 'B again'], ['mission/c', 'C']]);
    assert.equal(window.hasMore, false);
    assert.equal(window.snapshot.id, 'snapshot/later');
    assert.equal(applyWindow(window, { kind: 'resync', id: 'm' }), window);
});

test('glass methods preserve structure, null creation base, and idempotency headers', async () => {
    const fixture = require('../../../docs/st3/client-v0/fixtures/glasses.json');
    const put = require('../../../docs/st3/client-v0/fixtures/glass-put.json');
    const calls = [];
    const client = new St3Client({baseUrl: 'https://example.test', fetchImpl: async (url, options) => {
        calls.push({url, options});
        if (url.endsWith('/capabilities')) return response(envelope(capabilities));
        return response(fixture);
    }});
    const uuid = fixture.value.id.split('/').pop();
    assert.equal((await client.putGlass(uuid, put, 'create')).value.body.name, 'Main workspace');
    assert.deepEqual(JSON.parse(calls[0].options.body).body, put.body);
    assert.equal(calls[0].options.method, 'PUT');
    assert.equal(calls[0].options.headers['Idempotency-Key'], 'create');
    assert.equal(JSON.parse(calls[0].options.body).base_revision, null);
    await client.getGlass(fixture.value.id);
    assert.ok(calls.some(call => call.url.endsWith('/glasses/' + uuid)));
    await client.deleteGlass(uuid, {base_revision: fixture.value.revision}, 'delete');
    assert.equal(calls.at(-1).options.method, 'DELETE');
    assert.equal(calls.at(-1).options.headers['Idempotency-Key'], 'delete');
});

test('creation methods submit the shared typed fixtures', async () => {
    const calls = [];
    const client = new St3Client({ baseUrl: 'https://example.test', fetchImpl: async (url, init) => {
        calls.push({ url, init });
        if (url.endsWith('/capabilities')) return response(envelope(capabilities));
        return response(envelope({ kind: 'action-result', action_id: 'action/create', operation_id: 'operation/create', status: 'completed', affected_ids: ['agent/example/worker'], snapshot_id: snapshot.id }));
    } });
    for (const [fixture, method] of [['agent-create', 'agentCreate'], ['terminal-create', 'terminalCreate'], ['terminal-end', 'terminalEnd']]) {
        const request = require(`../../../docs/st3/client-v0/fixtures/${fixture}.json`);
        await client[method](request);
        assert.deepEqual(JSON.parse(calls.at(-1).init.body), request);
    }
});


test('search encodes text, filters, and cursor and preserves result targets', async () => {
    const calls = [];
    const fixture = require('../../../docs/st3/client-v0/fixtures/conversation-search.json');
    const client = new St3Client({ baseUrl: 'https://example.test', fetchImpl: async url => {
        calls.push(url);
        return response(url.endsWith('/capabilities') ? envelope(capabilities) : fixture);
    } });
    const search = await client.conversationSearch('café & orchid', { agent: 'agent/scribe', since: '2026-10-02T00:00:00Z', cursor: 'opaque+/=', limit: 2 });
    const url = new URL(calls[1]);
    assert.equal(url.pathname, '/v1/client/conversations/search');
    assert.equal(url.searchParams.get('text'), 'café & orchid');
    assert.equal(url.searchParams.get('agent'), 'agent/scribe');
    assert.equal(url.searchParams.get('since'), '2026-10-02T00:00:00Z');
    assert.equal(url.searchParams.get('cursor'), 'opaque+/=');
    assert.equal(search.value.items[0].entry_id, 'timeline-entry/note');
    assert.deepEqual(search.value.incomplete_sources, ['session/older: truncation']);
});

test('reads bounded seat status history through the paired gateway', async () => {
    const calls = [];
    const history = { kind: 'status-history', seat: 'agent/cedar', retained_from: '2026-10-04T12:00:00Z', complete: false,
        items: [{ seat: 'agent/cedar', runtime_incarnation: 'two', state: null, observed_at: '2026-10-04T12:00:00Z', reset: true }] };
    const client = new St3Client({ baseUrl: 'https://example.test', credential: () => 'proof', fetchImpl: async (url, init) => {
        calls.push({ url, init });
        return response(envelope(url.endsWith('/capabilities') ? capabilities : history));
    } });
    const result = await client.statusHistoryGet('agent/cedar');
    assert.equal(calls[1].url, 'https://example.test/v1/client/status-history/agent%2Fcedar');
    assert.equal(calls[1].init.headers.Authorization, 'Bearer proof');
    assert.equal(result.value.complete, false);
    assert.equal(result.value.items[0].reset, true);
});

test('canonical publication read encodes mission and schedule subjects and preserves defaults', async () => {
    const calls = [];
    const declaration = { max_active_runs: 1, revision_cutover: 'restart-active', steps: { inspect: { retry: { attempts: 1, backoff_ms: 0 } } } };
    const client = new St3Client({ baseUrl: 'https://example.test', fetchImpl: async (url) => {
        calls.push(url);
        return response(envelope(url.endsWith('/capabilities') ? capabilities : { kind: 'publication-definition', subject: new URL(url).searchParams.get('subject'), declaration, revision: 'a'.repeat(64), token: 'claim/example' }));
    } });
    for (const subject of ['mission/example/readback', 'schedule/example/daily']) {
        const result = await client.publicationDefinition(subject);
        assert.equal(result.value.subject, subject);
        assert.deepEqual(result.value.declaration, declaration);
        assert.equal(calls.at(-1), 'https://example.test/v1/client/publication-definition?subject=' + encodeURIComponent(subject));
    }
});

test('terminal exact filters encode subjects and refuse an old server ignoring filters', async () => {
    const calls = [];
    let filters = { owner: 'agent/lookup/seat-064', state: 'running' };
    const client = new St3Client({ baseUrl: 'https://example.test', fetchImpl: async url => {
        calls.push(url);
        return response(envelope(url.endsWith('/capabilities') ? capabilities : {
            kind: 'page', collection: 'terminals', filters, items: [], page: { limit: 1, has_more: false }
        }));
    } });
    await client.terminalsListFiltered({ ...filters, limit: 1 });
    assert.equal(calls.at(-1), 'https://example.test/v1/client/terminals?owner=agent%2Flookup%2Fseat-064&state=running&limit=1');
    filters = {};
    await assert.rejects(client.terminalsListFiltered({ owner: 'agent/lookup/seat-064' }), /upgrade the server/);
    await assert.rejects(client.terminalsListFiltered({ state: 'running' }), /upgrade the server/);
    await client.terminalsList();
    assert.equal(calls.at(-1), 'https://example.test/v1/client/terminals');
});

test('owner content chunk route encodes session identities and retains continuation offsets', async () => {
    const calls = [];
    const chunk = { kind: 'conversation-content-chunk', ref: 'a'.repeat(64), media_type: 'image/png', offset: 262144, size: 262145, data: 'Bw==', next_offset: null };
    const client = new St3Client({ baseUrl: 'https://example.test', fetchImpl: async (url, init) => {
        calls.push({ url, init });
        return response(envelope(url.endsWith('/capabilities') ? capabilities : chunk));
    } });
    const result = await client.conversationContentChunk('session/native/one', chunk.ref, chunk.offset);
    assert.equal(calls.at(-1).url, `https://example.test/v1/client/conversations/session%2Fnative%2Fone/content/${chunk.ref}/chunk?offset=262144`);
    assert.equal(calls.at(-1).init.headers['x-st3-features'], 'custom-subjects.v1, conversation-blocks.v1');
    assert.deepEqual(result.value, chunk);
});
