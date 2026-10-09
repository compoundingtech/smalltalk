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
    const frames = [];
    const ends = [];
    const client = new St3Client({ baseUrl: 'https://example.test/', credential: () => 'secret', fetchImpl: async () => { throw new Error('no HTTP'); } });
    const stream = await client.collectionStream({ onFrame: frame => frames.push(frame), onEnd: error => ends.push(error), socket: (url, protocols, headers) => { opened.push({ url, protocols, headers }); return socket; } });
    assert.deepEqual(opened, [{ url: 'wss://example.test/v1/client/collections/stream', protocols: ['st3.client.collections.v0'], headers: { Authorization: 'Bearer secret', 'x-st3-features': 'custom-subjects.v1, conversation-blocks.v1' } }]);
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
    assert.equal('membership' in window, false);
});

test('applyWindow keeps the newest membership state even when the window is unchanged', () => {
    const fixture = require('../../../fixtures/clients/ordered-memberships-v2.json');
    let window = applyWindow(undefined, fixture.snapshot_frame);
    assert.deepEqual(window.membership, fixture.snapshot_frame.membership);
    window = applyWindow(window, fixture.changes_frame);
    assert.deepEqual(window.items.map(item => item.id), ['mission/m1']);
    assert.deepEqual(window.membership, fixture.changes_frame.membership);
    const before = window;
    window = applyWindow(window, fixture.outside_window_changes_frame);
    assert.deepEqual(window.items, before.items);
    assert.equal(window.hasMore, before.hasMore);
    assert.deepEqual(window.membership, fixture.outside_window_changes_frame.membership);
    assert(window.membership.changed_index > before.membership.changed_index);
    assert.equal(applyWindow(window, { kind: 'resync', id: 'sidebar' }), window);
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
