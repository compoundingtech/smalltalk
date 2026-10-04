const { test } = require('node:test');
const assert = require('node:assert/strict');

// Node 24 loads the generated TypeScript directly, without a transpilation copy.
const modules = Promise.all([import('effect'), import('./Schema.generated.ts')]);

test('timestamp codecs reject normalized invalid calendar dates and preserve instants', async () => {
    const [{ DateTime, Schema }, Rich] = await modules;
    const decode = Rich.decodeUnknownSync(Rich.Timestamp);
    assert.equal(Schema.encodeSync(Rich.Timestamp)(decode('2024-02-29T12:00:00+02:00')), '2024-02-29T10:00:00.000Z');
    for (const invalid of ['2024-02-30T12:00:00Z', '1900-02-29T00:00:00Z', '2024-04-31T00:00:00Z', '2024-01-01T24:00:00Z']) {
        assert.throws(() => decode(invalid));
    }
    assert.equal(DateTime.formatIso(decode('2000-02-29T00:00:00Z')), '2000-02-29T00:00:00.000Z');
    assert.throws(() => Schema.encodeSync(Rich.Timestamp)(DateTime.makeUnsafe('+010000-01-01T00:00:00Z')));
});

test('unknown cases round-trip tolerantly and reject in both strict decoding APIs', async () => {
    const [{ Effect, Schema }, Rich] = await modules;
    const future = Rich.decodeUnknownSync(Rich.ErrorCode)('future-error');
    assert.deepEqual(future, { _tag: 'Unknown', raw: 'future-error' });
    assert.equal(Schema.encodeSync(Rich.ErrorCode)(future), 'future-error');
    assert.equal(Rich.decodeUnknownSync(Rich.ErrorCode, 'strict')('not-found'), 'not-found');
    assert.throws(() => Rich.decodeUnknownSync(Rich.ErrorCode, 'strict')('future-error'));
    assert.deepEqual(Effect.runSync(Rich.decodeUnknownEffect(Rich.ErrorCode)('future-error')), future);
    assert.throws(() => Effect.runSync(Rich.decodeUnknownEffect(Rich.ErrorCode, 'strict')('future-error')));
    assert.throws(() => Schema.encodeSync(Rich.ErrorCode)({ _tag: 'Unknown', raw: 'not-found' }));
});

test('recursive Glass layouts enforce child bounds and strict nested keys', async () => {
    const [{ Schema }, Rich] = await modules;
    const layout = { split: 'right', children: [{ tabs: [{ pane: 'left' }] }, { tabs: [{ pane: 'right' }] }] };
    assert.deepEqual(Rich.decodeUnknownSync(Rich.GlassLayout, 'strict')(layout), layout);
    assert.deepEqual(Schema.encodeSync(Rich.GlassLayout)(layout), layout);
    assert.throws(() => Rich.decodeUnknownSync(Rich.GlassLayout)({ split: 'right', children: [{ tabs: [] }] }));
    const extra = { tabs: [{ pane: 'left', future: true }] };
    assert.deepEqual(Rich.decodeUnknownSync(Rich.GlassLayout)(extra), { tabs: [{ pane: 'left' }] });
    assert.throws(() => Rich.decodeUnknownSync(Rich.GlassLayout, 'strict')(extra));
});

test('nullable Work timing codecs equate missing and null while rejecting unsafe wire integers', async () => {
    const [{ DateTime, Duration, Option, Schema }, Rich] = await modules;
    const wire = {
        id: 'work/test', kind: 'work', revision: '1', updated_at: '2024-02-29T00:00:00Z',
        mission_run_id: 'mission-run/test', generation_id: 'run-generation/test', definition_id: '1',
        path: 'test', state: 'ready', attempt: 0, readiness_epoch: 0, goals: [], constraints: [],
    };
    const decode = Rich.decodeUnknownSync(Rich.Work, 'strict');
    const absent = decode(wire);
    const nullable = decode({ ...wire, claim_expires_at_unix_ms: null, execution_started_at_unix_ms: null, timeout_ms: null });
    assert.deepEqual(absent, nullable);
    assert(Option.isNone(absent.claim_expires_at_unix_ms));
    assert.equal(Schema.encodeSync(Rich.Work)(absent).claim_expires_at_unix_ms, null);
    const timed = decode({ ...wire, claim_expires_at_unix_ms: 1000, execution_elapsed_ms: 12, timeout_ms: 15 });
    assert(Option.isSome(timed.claim_expires_at_unix_ms));
    assert.equal(DateTime.toEpochMillis(timed.claim_expires_at_unix_ms.value), 1000);
    assert.equal(Duration.toMillis(timed.execution_elapsed_ms), 12);
    assert(Option.isSome(timed.timeout_ms));
    assert.equal(Duration.toMillis(timed.timeout_ms.value), 15);
    for (const invalid of [-1, 1.5, Number.MAX_SAFE_INTEGER + 1]) {
        assert.throws(() => decode({ ...wire, execution_elapsed_ms: invalid }));
    }
    assert.throws(() => decode({ ...wire, claim_expires_at_unix_ms: 253402300800000 }));
    assert.throws(() => Schema.encodeSync(Rich.Work)({ ...timed, attempt: Number.MAX_SAFE_INTEGER + 1 }));
    assert.throws(() => Schema.encodeSync(Rich.Work)({ ...timed, execution_elapsed_ms: Duration.infinity }));
    const tolerated = Rich.decodeUnknownSync(Rich.Work)({ ...wire, agentless: null });
    assert.equal(Object.hasOwn(tolerated, 'agentless'), false);
    assert.equal(Object.hasOwn(Schema.encodeSync(Rich.Work)(tolerated), 'agentless'), false);
    assert.throws(() => decode({ ...wire, agentless: null }));
});

test('second durations preserve safe wire units without millisecond precision loss', async () => {
    const [{ Duration, Option, Schema }, Rich] = await modules;
    const wire = { host_id: 'host/test', peer_only_envelopes: 0, local_only_envelopes: 0 };
    for (const seconds of [0, 2, Number.MAX_SAFE_INTEGER - 2, Number.MAX_SAFE_INTEGER]) {
        const decoded = Rich.decodeUnknownSync(Rich.SyncPeer, 'strict')({ ...wire, estimated_catch_up_seconds: seconds });
        assert(Option.isSome(decoded.estimated_catch_up_seconds));
        assert.equal(Duration.toNanosUnsafe(decoded.estimated_catch_up_seconds.value), BigInt(seconds) * 1_000_000_000n);
        assert.equal(Schema.encodeSync(Rich.SyncPeer)(decoded).estimated_catch_up_seconds, seconds);
    }
    const decoded = Rich.decodeUnknownSync(Rich.SyncPeer)(wire);
    assert.equal(Schema.encodeSync(Rich.SyncPeer)({ ...decoded, estimated_catch_up_seconds: Option.some(Duration.millis(2500)) }).estimated_catch_up_seconds, 3);
    assert.throws(() => Rich.decodeUnknownSync(Rich.SyncPeer)({ ...wire, estimated_catch_up_seconds: Number.MAX_SAFE_INTEGER + 1 }));
    assert.throws(() => Schema.encodeSync(Rich.SyncPeer)({ ...decoded, estimated_catch_up_seconds: Option.some(Duration.nanos((BigInt(Number.MAX_SAFE_INTEGER) + 1n) * 1_000_000_000n)) }));
});

test('strict mode distinguishes real unknown enums from arbitrary same-shaped JSON bags', async () => {
    const [{ Schema }, Rich] = await modules;
    const payload = Schema.Struct({
        details: Schema.Unknown,
        codes: Schema.Array(Rich.ErrorCode),
        optional: Schema.OptionFromOptionalNullOr(Rich.ErrorCode, { onNoneEncoding: null }),
    });
    const raw = { _tag: 'Unknown', raw: 'user-data' };
    const cyclic = { ...raw };
    cyclic.self = cyclic;
    assert.equal(Rich.containsUnknownCase(raw), false);
    assert.equal(Rich.containsUnknownCase(cyclic), false);
    assert.equal(Rich.decodeUnknownSync(payload, 'strict')({ details: cyclic, codes: ['not-found'] }).details, cyclic);
    const unknown = Rich.decodeUnknownSync(payload)({ details: raw, codes: ['future-error'] });
    assert.equal(Rich.containsUnknownCase(unknown), true);
    assert.throws(() => Rich.decodeUnknownSync(payload, 'strict')({ details: raw, codes: ['future-error'] }));
    assert.throws(() => Rich.decodeUnknownSync(payload, 'strict')({ details: raw, codes: [], optional: 'future-error' }));
});

test('optional string null is absent only tolerantly, while required boolean null always fails', async () => {
    const [{ Schema }, Rich] = await modules;
    const absent = { pane: 'test' };
    const tolerated = Rich.decodeUnknownSync(Rich.GlassTab)({ ...absent, title: null });
    assert.deepEqual(tolerated, absent);
    assert.deepEqual(Schema.encodeSync(Rich.GlassTab)(tolerated), absent);
    assert.throws(() => Rich.decodeUnknownSync(Rich.GlassTab, 'strict')({ ...absent, title: null }));
    const glass = {
        id: 'glass/test', kind: 'glass', revision: '1', updated_at: '2024-02-29T00:00:00Z',
        body: null, deleted: null, base_revision: null, replaced_revision: null,
    };
    for (const mode of ['tolerant', 'strict']) {
        const decode = Rich.decodeUnknownSync(Rich.Glass, mode);
        assert.equal(decode({ ...glass, deleted: false }).deleted, false);
        assert.throws(() => decode(glass));
    }
});

test('timeline discrimination preserves required headers and conditional bodies', async () => {
    const [{ Schema }, Rich] = await modules;
    const wire = {
        id: 'timeline-entry/test', sequence: 1, revision: 1, timestamp: '2024-02-29T00:00:00.000Z',
        role: 'assistant', type: 'status', final: false, body: { status: 'running' },
    };
    for (const mode of ['tolerant', 'strict']) {
        const decode = Rich.decodeUnknownSync(Rich.TimelineEntry, mode);
        assert.equal(decode(wire).body.status, 'running');
        assert.throws(() => decode({ ...wire, body: {} }));
        assert.throws(() => decode({ ...wire, id: undefined }));
        assert.throws(() => decode({ ...wire, type: 'message', body: {} }));
    }
    const future = { ...wire, type: 'future-entry', body: { new_field: true } };
    const decoded = Rich.decodeUnknownSync(Rich.TimelineEntry)(future);
    assert.deepEqual(decoded.type, { _tag: 'Unknown', raw: 'future-entry' });
    assert.deepEqual(Schema.encodeSync(Rich.TimelineEntry)(decoded), future);
    assert.throws(() => Rich.decodeUnknownSync(Rich.TimelineEntry, 'strict')(future));
});

test('subject references validate complete family identities while brands preserve wire strings', async () => {
    const [{ Schema }, Rich] = await modules;
    const decode = Rich.decodeUnknownSync(Rich.MissionRunId, 'strict');
    const id = 'mission-run/test/nested';
    assert.equal(Schema.encodeSync(Rich.MissionRunId)(decode(id)), id);
    for (const invalid of ['mission/test', 'mission-run/', 'mission-run/test value']) {
        assert.throws(() => decode(invalid));
    }
    const cursor = 'opaque-cursor';
    assert.equal(Schema.encodeSync(Rich.Cursor)(Rich.decodeUnknownSync(Rich.Cursor)(cursor)), cursor);
    assert.throws(() => Rich.decodeUnknownSync(Rich.Cursor)('short'));
});

test('paired credentials redact decoded values and restore the original wire secret', async () => {
    const [{ Redacted, Schema }, Rich] = await modules;
    const wire = {
        kind: 'paired-session', device_id: 'device/test', person_id: 'person/test',
        session_actor: 'session/test', credential: 'synthetic-credential-00000000000000',
        scopes: ['read'], expires_at: '2024-02-29T00:00:00.000Z',
    };
    const decoded = Rich.decodeUnknownSync(Rich.PairedSession, 'strict')(wire);
    assert(Redacted.isRedacted(decoded.credential));
    assert.equal(Redacted.value(decoded.credential), wire.credential);
    assert.equal(String(decoded.credential).includes(wire.credential), false);
    assert.deepEqual(Schema.encodeSync(Rich.PairedSession)(decoded), wire);
    assert.throws(() => Rich.decodeUnknownSync(Rich.PairedSession)({ ...wire, credential: 'short' }));
    assert.throws(() => Schema.encodeSync(Rich.PairedSession)({ ...decoded, credential: Redacted.make('short') }));
});

test('future resource kinds cannot conceal malformed known resources', async () => {
    const [{ Schema }, Rich] = await modules;
    const wire = {
        id: 'future-resource/test', kind: 'future-resource', revision: '1',
        updated_at: '2024-02-29T00:00:00.000Z',
    };
    const decoded = Rich.decodeUnknownSync(Rich.Resource)(wire);
    assert.deepEqual(decoded.kind, { _tag: 'Unknown', raw: 'future-resource' });
    assert.deepEqual(Schema.encodeSync(Rich.Resource)(decoded), wire);
    assert.throws(() => Rich.decodeUnknownSync(Rich.Resource, 'strict')(wire));
    for (const kind of ['work', 'mission', 'glass']) {
        assert.throws(() => Rich.decodeUnknownSync(Rich.Resource)({ ...wire, kind }));
    }
});

test('mission generation maps require complete run identities as keys', async () => {
    const [{ Schema }, Rich] = await modules;
    const wire = {
        id: 'mission/test', kind: 'mission', revision: '1', updated_at: '2024-02-29T00:00:00.000Z',
        title: 'Test', state: 'ready', mission_revision: '1', runs: ['mission-run/test'],
        run_generations: { 'mission-run/test': 'run-generation/test' }, visualization: null, usage: null,
    };
    const decode = Rich.decodeUnknownSync(Rich.Mission, 'strict');
    assert.deepEqual(Schema.encodeSync(Rich.Mission)(decode(wire)), wire);
    for (const key of ['mission-run/', 'mission/test', 'mission-run/has space']) {
        assert.throws(() => decode({ ...wire, run_generations: { [key]: 'run-generation/test' } }));
    }
});

test('resource pages decode only through ResourcesPage, never the generic Page', async () => {
    const [, Rich] = await modules;
    const page = (collection) => ({ kind: 'page', collection, filters: {}, items: [], page: { limit: 50, has_more: false } });
    assert.equal(Rich.decodeUnknownSync(Rich.Page, 'strict')(page('missions')).collection, 'missions');
    assert.throws(() => Rich.decodeUnknownSync(Rich.Page)(page('resources')));
});

test('arrangement subscriptions require a concrete owner and placements preserve explicit root', async () => {
    const [{ Schema, Option }, Rich] = await modules;
    const command = { kind: 'subscribe', id: 'sidebar', collection: 'arrangements', person: 'person/alice', limit: 100 };
    const decodeCommand = Rich.decodeUnknownSync(Rich.CollectionCommand, 'strict');
    assert.equal(decodeCommand(command).person, 'person/alice');
    for (const person of [undefined, null, 'agent/alice', 'person/alice/other']) {
        assert.throws(() => decodeCommand({ ...command, person }));
    }
    const operation = { op: 'subject.place', subject: 'agent/alice/worker', folder: null, key: 'a0' };
    const decoded = Rich.decodeUnknownSync(Rich.ArrangementOperation, 'strict')(operation);
    assert(Option.isNone(decoded.folder));
    assert.deepEqual(Schema.encodeSync(Rich.ArrangementOperation)(decoded), operation);
    assert.throws(() => Rich.decodeUnknownSync(Rich.ArrangementOperation, 'strict')({ ...operation, folder: 'not-a-folder-id' }));
    for (const subject of ['pty/person/alice/019a0000-0000-7000-8000-000000000001', 'session/worker']) {
        assert.throws(() => Rich.decodeUnknownSync(Rich.ArrangementOperation, 'strict')({ ...operation, subject }));
    }
    assert.throws(() => Rich.decodeUnknownSync(Rich.ArrangementOperation, 'strict')({ ...operation, parent: null }));
    assert.throws(() => Rich.decodeUnknownSync(Rich.ArrangementTombstoneRegister, 'strict')({ value: false, revision: 'claim/deleted' }));
});

test('arrangement resources keep deleted folder positions and missing-seat placements distinct from resolved locations', async () => {
    const [{ Schema }, Rich] = await modules;
    const fixture = require('../../../fixtures/clients/arrangements-v1.json');
    const encoded = Schema.encodeSync(Rich.Arrangement)(Rich.decodeUnknownSync(Rich.Arrangement, 'strict')(fixture.resource));
    const deletedID = Object.keys(encoded.body.folders).find(id => encoded.body.folders[id].tombstone !== null);
    assert.equal(encoded.body.folders[deletedID].position.value.key, 'a0');
    assert.equal(encoded.body.placements['agent/ada/worker'].value.folder, deletedID);
    assert.equal(encoded.resolved.folders['agent/ada/worker'], null);
    assert.equal(encoded.body.placements['agent/ada/offline'].value.key, 'a1');
    assert.equal(encoded.body.folders[deletedID].tombstone.value, true);
});
