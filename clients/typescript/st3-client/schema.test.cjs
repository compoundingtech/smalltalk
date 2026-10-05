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

const nativeContracts = Promise.all([
    import('./index.ts'),
    Promise.resolve().then(() => require('../../../docs/st3/client-v0/schemas/subject-projection.schema.json')),
]);
const nativeProvenance = {
    source: 'replicated', claim_id: 'claim/native-proof', origin: 'host/proof',
    accepted_at: '2026-10-05T00:00:00.000Z', store_index: 1,
};
const nativeClaim = (entry, kind, fields) => ({
    id: nativeProvenance.claim_id, ref: `${entry.family}/proof`, kind,
    schema_id: entry.claim_schema_ids[kind], retention: entry.descriptor.claims[kind].retention,
    provenance: nativeProvenance, payload_availability: 'available', fields, omitted_fields: [],
});
const changedHash = (id) => {
    const start = 'subject-claim-schema/'.length;
    return id.slice(0, start) + (id[start] === '0' ? '1' : '0') + id.slice(start + 1);
};
const unavailableClaim = (claim) => ({
    kind: 'unsupported-subject-schema', id: claim.id, ref: claim.ref,
    schema_id: claim.schema_id, payload_availability: 'unsupported-schema',
});

test('native subjects decode every registry family and reject changed descriptor contracts', async () => {
    const [{ Schema, Effect }, Rich] = await modules;
    const [Raw, artifact] = await nativeContracts;
    const catalog = {
        kind: 'subject-schemas', wire_version: artifact.wire_version, families: artifact.families,
        native_registry_digest: artifact.native_registry_digest,
        projected_schema_sha256: artifact.projected_schema_sha256,
        projected_digest_convention: artifact.projected_digest_convention,
        resources: artifact.resources,
        projected_json_schema: artifact.$defs.SubjectSchemas.properties.projected_json_schema.const,
    };
    assert.deepEqual(Raw.decodeSubjectSchemas(catalog), catalog);
    const richCatalog = Effect.runSync(Schema.decodeUnknownEffect(Rich.SubjectSchemas, { onExcessProperty: 'error' })(catalog));
    assert.deepEqual(Schema.encodeSync(Rich.SubjectSchemas)(richCatalog), catalog);
    for (const [index, entry] of artifact.families.entries()) {
        const ref = entry.family === 'file' ? 'file/proof:/proof'
            : entry.family === 'custom' ? 'custom/team/proof' : `${entry.family}/proof`;
        const subject = {
            kind: 'subject', id: ref, ref, family: entry.family, schema_id: entry.schema_id,
            heads: [], heads_complete: false, local_fence: { node: 'host/proof', position: 7 },
        };
        assert.deepEqual(Raw.decodeSubjectProjection(subject), subject);
        const rich = Effect.runSync(Rich.decodeNativeSubjectProjection(subject));
        assert.deepEqual(Schema.encodeSync(Rich.SUBJECT_FAMILY_CODECS[entry.schema_id])(rich), subject);
        assert.throws(() => Raw.decodeSubjectProjection({ ...subject, id: `${ref}/different` }), Raw.SubjectDecodeError);
        const changed = structuredClone(catalog);
        changed.families[index].descriptor.identity = { unreviewed_access: true };
        assert.throws(() => Raw.decodeSubjectSchemas(changed), Raw.UnsupportedSubjectDescriptorError);
        assert.throws(() => Rich.decodeUnknownSync(Rich.SubjectSchemas, 'strict')(changed));
    }
});

test('native message claims preserve absent and null fields and reject wrong scalars and references', async () => {
    const [{ Schema, Effect }, Rich] = await modules;
    const [Raw, artifact] = await nativeContracts;
    const entry = artifact.families.find(entry => entry.family === 'message');
    const absent = nativeClaim(entry, 'message.sent', { from: 'person/alex', to: 'agent/proof', status: 'sent' });
    const nullable = { ...absent, fields: { ...absent.fields, content: null, title: null, tags: null } };
    for (const wire of [absent, nullable]) {
        assert.deepEqual(Raw.decodeSubjectClaim(wire), wire);
        const decoded = Effect.runSync(Rich.decodeNativeSubjectClaim(wire));
        const encoded = Schema.encodeSync(Rich.SUBJECT_CLAIM_CODECS[wire.schema_id])(decoded);
        assert.deepEqual(encoded, wire);
        assert.equal(Object.hasOwn(decoded.fields, 'content'), Object.hasOwn(wire.fields, 'content'));
    }
    for (const fields of [
        { ...absent.fields, content: 42 },
        { ...absent.fields, to: 'person/' },
        { ...absent.fields, status: 'future-status' },
        { ...absent.fields, hidden_payload: 'must not become typed' },
    ]) {
        const malformed = { ...absent, fields };
        assert.throws(() => Raw.decodeSubjectClaim(malformed), Raw.SubjectDecodeError);
        await assert.rejects(Effect.runPromise(Rich.decodeNativeSubjectClaim(malformed)));
    }
    const changed = { ...absent, schema_id: changedHash(absent.schema_id), fields: { unvalidated: 'never disclose' } };
    assert.deepEqual(Raw.decodeSubjectClaim(changed), unavailableClaim(changed));
    assert.deepEqual(Effect.runSync(Rich.decodeNativeSubjectClaim(changed)), unavailableClaim(changed));
    const ref = absent.ref;
    const subject = {
        kind: 'subject', id: ref, ref, family: entry.family, schema_id: entry.schema_id,
        heads: [changed], heads_complete: true, local_fence: { node: 'host/proof', position: 0 },
    };
    const unavailable = {
        kind: 'unsupported-subject-schema', id: ref, ref, schema_id: entry.schema_id,
        payload_availability: 'unsupported-schema',
    };
    assert.deepEqual(Raw.decodeSubjectProjection(subject), unavailable);
    assert.deepEqual(Effect.runSync(Rich.decodeNativeSubjectProjection(subject)), unavailable);
});

test('unknown native head descriptors still require canonical projection and claim headers', async () => {
    const [{ Effect }, Rich] = await modules;
    const [Raw, artifact] = await nativeContracts;
    const entry = artifact.families.find(entry => entry.family === 'message');
    const claim = {
        ...nativeClaim(entry, 'message.sent', { unvalidated: 'never disclose' }),
        schema_id: 'future-head-descriptor',
    };
    const subject = {
        kind: 'subject', id: claim.ref, ref: claim.ref, family: entry.family, schema_id: entry.schema_id,
        heads: [claim], heads_complete: true, local_fence: { node: 'host/proof', position: 0 },
    };
    assert.deepEqual(Raw.decodeSubjectProjection(subject), unavailableClaim(subject));
    assert.deepEqual(Effect.runSync(Rich.decodeNativeSubjectProjection(subject)), unavailableClaim(subject));
    assert.deepEqual(Raw.decodeSubjectClaim(claim), unavailableClaim(claim));
    for (const malformed of [
        { ...subject, id: 'message/different' },
        { ...subject, id: 'message/', ref: 'message/' },
        { ...subject, local_fence: { node: 'host/proof', position: -1 } },
        { ...subject, heads: [{ ...claim, ref: 'message/' }] },
        { ...subject, heads: [{ ...claim, provenance: undefined }] },
    ]) {
        assert.throws(() => Raw.decodeSubjectProjection(malformed), Raw.SubjectDecodeError);
        await assert.rejects(Effect.runPromise(Rich.decodeNativeSubjectProjection(malformed)));
    }
    for (const malformed of [
        { ...claim, ref: 'message/' },
        { ...claim, id: '' },
        { ...claim, retention: 'future-retention' },
        { ...claim, provenance: undefined },
    ]) {
        assert.throws(() => Raw.decodeSubjectClaim(malformed), Raw.SubjectDecodeError);
        await assert.rejects(Effect.runPromise(Rich.decodeNativeSubjectClaim(malformed)));
    }
});

test('native concrete custom hashes match registry descriptors and reject cross-kind substitution', async () => {
    const [{ Schema, Effect }, Rich] = await modules;
    const [Raw, artifact] = await nativeContracts;
    const { createHash } = require('node:crypto');
    const entry = artifact.families.find(entry => entry.family === 'custom');
    const canonical = value => Array.isArray(value) ? '[' + value.map(canonical).join(',') + ']'
        : value !== null && typeof value === 'object'
            ? '{' + Object.keys(value).sort().map(key => JSON.stringify(key) + ':' + canonical(value[key])).join(',') + '}'
            : JSON.stringify(value);
    const nativeId = kind => {
        const effective = {
            wire_version: artifact.wire_version, family: entry.family, kind,
            identity: entry.descriptor.identity, claim: entry.descriptor.claims['custom.*'],
            value_semantics: entry.descriptor.value_semantics, custom_payload: entry.descriptor.custom_payload,
        };
        return `subject-claim-schema/${createHash('sha256').update(canonical(effective)).digest('hex')}/custom/${kind}`;
    };
    const kind = 'custom.team.note';
    assert.equal(Raw.customClaimSchemaId(kind), nativeId(kind));
    assert.equal(Raw.customClaimSchemaId('custom.team.other'), nativeId('custom.team.other'));
    const claim = {
        id: nativeProvenance.claim_id, ref: 'custom/team/proof', kind, schema_id: nativeId(kind),
        retention: 'durable', provenance: nativeProvenance, payload_availability: 'available',
        fields: { explicit_null: null, nested: { flags: [false, 0, 'text'] } }, omitted_fields: [],
    };
    assert.deepEqual(Raw.decodeSubjectClaim(claim), claim);
    const decoded = Rich.decodeUnknownSync(Rich.NativeCustomCustomClaim, 'strict')(claim);
    assert.deepEqual(Schema.encodeSync(Rich.NativeCustomCustomClaim)(decoded), claim);
    const otherHash = nativeId('custom.team.other').split('/')[1];
    for (const schema_id of [changedHash(claim.schema_id), `subject-claim-schema/${otherHash}/custom/${kind}`]) {
        const mismatched = { ...claim, schema_id };
        assert.throws(() => Schema.decodeUnknownSync(Rich.NativeCustomCustomClaim)(mismatched));
        assert.throws(() => Schema.decodeUnknownSync(Rich.SUBJECT_CUSTOM_CLAIM_CODEC)(mismatched));
        assert.deepEqual(Raw.decodeSubjectClaim(mismatched), unavailableClaim(mismatched));
        assert.deepEqual(Effect.runSync(Rich.decodeNativeSubjectClaim(mismatched)), unavailableClaim(mismatched));
    }
    for (const ref of ['custom/client', 'custom/client/private', 'agent/proof']) {
        assert.throws(() => Schema.decodeUnknownSync(Rich.NativeCustomCustomClaim)({ ...claim, ref }));
    }
    assert.equal(Raw.customClaimSchemaId('custom.client.private'), undefined);
});

test('usage pricing provenance round-trips while legacy rows remain readable', async () => {
    const [{ Schema }, Rich] = await modules;
    const counts = { total_tokens: 1100, input_tokens: 1000, output_tokens: 100,
        cache_write_tokens: 0, cache_write_1h_tokens: 0, cached_tokens: 0,
        cost_microusd: 3000, reported_cost_microusd: 0, unpriced_tokens: 0 };
    const legacy = { agent: 'agent/example.usage', pricing: 'old-label', ...counts };
    const decode = Rich.decodeUnknownSync(Rich.UsageRow, 'strict');
    assert.deepEqual(Schema.encodeSync(Rich.UsageRow)(decode(legacy)), legacy);
    const wire = { ...legacy, native_session_id: 'native-example', pricing_provenance: [{
        price_table_id: 'st.api-list', price_table_version: 'example-version', cost_source: 'computed',
        rates_usd_per_million_tokens: { input: 2, output: 10, cache_read: 0.1, cache_write_5m: 2.5, cache_write_1h: 2.5 },
        ...counts,
    }] };
    assert.deepEqual(Schema.encodeSync(Rich.UsageRow)(decode(wire)), wire);
    for (const cost_source of ['provider_reported', 'unpriced']) {
        const value = { ...legacy, pricing_provenance: [{ cost_source, ...counts }] };
        assert.deepEqual(Schema.encodeSync(Rich.UsageRow)(decode(value)), value);
    }
    assert.throws(() => decode({ ...wire, pricing_provenance: [{ ...wire.pricing_provenance[0], cost_source: 'invented' }] }));
    assert.throws(() => decode({ ...wire, pricing_provenance: [{ ...wire.pricing_provenance[0], rates_usd_per_million_tokens: { input: 2 } }] }));
});

test('usage identity metadata is additive and independent of quota age', async () => {
    const [{ Schema }, Rich] = await modules;
    const wire = { account: 'claude/unknown', driver: 'claude', measured_at_unix_ms: 1000, measured_by: 'agent/example/worker', host: 'alder', seats: [] };
    const decode = Rich.decodeUnknownSync(Rich.UsageLimit, 'strict');
    assert.equal(Object.hasOwn(decode(wire), 'identified'), false);
    for (const identified of [false, true]) {
        const decoded = decode({ ...wire, identified });
        assert.equal(decoded.identified, identified);
        assert.equal(Schema.encodeSync(Rich.UsageLimit)(decoded).identified, identified);
    }
    assert.throws(() => decode({ ...wire, identified: 'stale' }));
});
