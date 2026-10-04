import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { St3Client } from './Client.generated';

test('uses the global receiver for the default browser fetch', async () => {
    const originalFetch = globalThis.fetch;
    globalThis.fetch = async function (this: typeof globalThis, url, init) {
        if (this !== globalThis) throw new TypeError('Illegal invocation');
        assert.equal(url, 'https://example.test/v1/client/capabilities');
        assert.equal(init?.method, 'GET');
        return Response.json({
            api_version: 'st3.client.v0',
            request_id: 'request/receiver',
            snapshot: { id: 'snapshot/test', host_id: 'host/test', store_index: 1, projection_version: 'client-projection.v0', created_at: '2026-09-20T00:00:00Z' },
            value: {},
        });
    };
    try {
        const client = new St3Client({ baseUrl: 'https://example.test/' });
        const result = await client.capabilities();
        assert.equal(result.request_id, 'request/receiver');
    } finally {
        globalThis.fetch = originalFetch;
    }
});
