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
const { St3Client, ClientError } = require(path.join(temporary, 'Client.generated.js'));
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

test('generated hash uses normative schema and operations bytes', () => {
    const crypto = require('node:crypto');
    const root = path.join(__dirname, '../../..');
    const hash = crypto.createHash('sha256');
    hash.update(fs.readFileSync(path.join(root, 'docs/st3/client-v0/schemas/client-v0.schema.json')));
    hash.update(fs.readFileSync(path.join(root, 'docs/st3/client-v0/schemas/operations.json')));
    assert.equal(CONTRACT_SHA256, hash.digest('hex'));
});
