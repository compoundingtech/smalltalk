const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const ts = require('../../../apps/ios/node_modules/typescript');

const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'st3-ts-client-'));
for (const name of ['Models.generated', 'Client.generated']) {
    const source = fs.readFileSync(path.join(__dirname, `${name}.ts`), 'utf8');
    const output = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } }).outputText;
    fs.writeFileSync(path.join(temporary, `${name}.js`), output);
}
const { St3Client, ClientError, applyWindow } = require(path.join(temporary, 'Client.generated.js'));
const { CONTRACT_SHA256 } = require(path.join(temporary, 'Models.generated.js'));
fs.rmSync(temporary, { recursive: true, force: true });

const snapshot = { id: 'snapshot/test', host_id: 'host/test', store_index: 1, projection_version: 'client-projection.v0', created_at: '2026-09-20T00:00:00Z' };
function envelope(value) { return { api_version: 'st3.client.v0', request_id: 'request/test', snapshot, value }; }
function response(value, status = 200) { return { ok: status < 400, status, json: async () => value }; }
const capabilityFixture = require('../../../docs/st3/client-v0/fixtures/capabilities.json');
const cursorGapFixture = require('../../../docs/st3/client-v0/fixtures/cursor-gap-error.json');
const capabilities = { ...capabilityFixture.value, limits: { ...capabilityFixture.value.limits, max_page_items: 2, max_event_items: 3, max_wait_ms: 10 } };

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
        headers: { Authorization: 'Bearer secret' },
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
    assert.deepEqual(opened, [{ url: 'wss://example.test/v1/client/conversations/example/stream?after=conversation-cursor%2Fowner%2Fexample%2F1.2.3', protocols: ['st3.client.conversation.v0'], headers: { Authorization: 'Bearer secret' } }]);
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
    const frames = [];
    const ends = [];
    const client = new St3Client({ baseUrl: 'https://example.test/', credential: () => 'secret', fetchImpl: async () => { throw new Error('no HTTP'); } });
    const stream = await client.collectionStream({ onFrame: frame => frames.push(frame), onEnd: error => ends.push(error), socket: (url, protocols, headers) => { opened.push({ url, protocols, headers }); return socket; } });
    assert.deepEqual(opened, [{ url: 'wss://example.test/v1/client/collections/stream', protocols: ['st3.client.collections.v0'], headers: { Authorization: 'Bearer secret' } }]);
    stream.subscribe('missions', 'missions', 200);
    stream.subscribe('mine', 'attention', 50, { person: 'person/example' });
    assert.deepEqual(socket.sent, []);
    socket.onopen();
    stream.subscribeTerminal('term', 'terminal/example', 'pty-1:2026-09-20T00:00:00Z', 'capability-proof');
    stream.subscribeConversation('talk', 'agent/example');
    stream.unsubscribe('talk');
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

test('generated hash uses normative schema and operations bytes', () => {
    const crypto = require('node:crypto');
    const root = path.join(__dirname, '../../..');
    const hash = crypto.createHash('sha256');
    hash.update(fs.readFileSync(path.join(root, 'docs/st3/client-v0/schemas/client-v0.schema.json')));
    hash.update(fs.readFileSync(path.join(root, 'docs/st3/client-v0/schemas/operations.json')));
    assert.equal(CONTRACT_SHA256, hash.digest('hex'));
});
