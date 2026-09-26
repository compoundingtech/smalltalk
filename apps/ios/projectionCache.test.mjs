import assert from 'node:assert/strict';
import { decodeProjectionCache, emptyData, encodeProjectionCache, hydrateProjectionForPairedDevice, offlinePresentation } from './projectionCache.ts';

const gateway = 'https://example.invalid';
const now = Date.UTC(2026, 8, 24);
const data = {
  ...emptyData,
  attention: [{ id: 'attention/1', kind: 'attention', title: 'Review', credential: 'must-not-persist', nested: { preview_token: 'also-secret' } }],
  sessions: [{ id: 'session/1', kind: 'session', owner_id: 'agent/one', state: 'running' }],
};
const encoded = encodeProjectionCache(gateway, 'person/one', 'host/hetz', 42, data, now);
assert.ok(encoded);
assert.equal(encoded.includes('must-not-persist'), false);
assert.equal(encoded.includes('also-secret'), false);
const hydrated = decodeProjectionCache(encoded, gateway, now + 1000);
assert.equal(hydrated?.actor, 'person/one');
assert.equal(hydrated?.storeIndex, 42);
assert.equal(hydrated?.data.sessions[0].id, 'session/1');
assert.equal(hydrateProjectionForPairedDevice(encoded, gateway, false, now), null);
assert.equal(hydrateProjectionForPairedDevice(encoded, gateway, true, now)?.data.attention[0].title, 'Review');
assert.equal(decodeProjectionCache(encoded, 'https://other.invalid', now), null);
assert.equal(decodeProjectionCache(encoded, gateway, now + 8 * 24 * 60 * 60 * 1000), null);
assert.equal(decodeProjectionCache(encoded, gateway, now - 6 * 60 * 1000), null);
assert.equal(decodeProjectionCache(encoded.replace('"version":2', '"version":1'), gateway, now), null);
assert.equal(decodeProjectionCache('{bad json', gateway, now), null);
assert.equal(decodeProjectionCache(encoded.replace('"kind":"session"', '"kind":"terminal-attachment"'), gateway, now), null);
assert.equal(offlinePresentation(false).title, 'Offline');
assert.match(offlinePresentation(false).detail, /No data is cached/);
assert.match(offlinePresentation(true).title, /showing last data/);

const excessive = { ...emptyData, sessions: Array.from({ length: 110 }, (_, i) => ({ id: `session/${i}`, kind: 'session' })) };
const bounded = decodeProjectionCache(encodeProjectionCache(gateway, 'person/one', 'host/hetz', 42, excessive, now), gateway, now);
assert.equal(bounded?.data.sessions.length, 100);
assert.deepEqual(bounded?.truncated, ['sessions']);
const serverTruncated = decodeProjectionCache(encodeProjectionCache(gateway, 'person/one', 'host/hetz', 42, emptyData, now, ['attention']), gateway, now);
assert.deepEqual(serverTruncated?.truncated, ['attention']);

// A direct Tailscale HTTP gateway gets the same bounded offline cache as HTTPS.
const tailnet = 'http://100.64.0.1:4102';
const tailnetCache = encodeProjectionCache(tailnet, 'person/one', 'host/hetz', 42, data, now);
assert.ok(tailnetCache);
assert.equal(decodeProjectionCache(tailnetCache, tailnet, now)?.gateway, tailnet);
