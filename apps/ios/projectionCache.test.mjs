import assert from 'node:assert/strict';
import { decodeProjectionCache, emptyData, encodeProjectionCache, hydrateProjectionForPairedDevice, offlinePresentation } from './projectionCache.ts';

const gateway = 'https://example.invalid';
const now = Date.UTC(2026, 8, 24);
const data = {
  ...emptyData,
  attention: [{ id: 'attention/1', kind: 'attention', title: 'Review', credential: 'must-not-persist', nested: { preview_token: 'also-secret' } }],
  sessions: [{ id: 'session/1', kind: 'session', owner_id: 'agent/one', state: 'running' }],
  missions: [{ id: 'mission/1', kind: 'mission', title: 'fleet/app/release', run_details: [{ id: 'mission-run/1', steps: [{ id: 'step-run/1/build', path: 'build', state: 'claimed' }] }] }],
  agents: [{ id: 'agent/one', kind: 'agent', name: 'fleet/app', next_work: { id: 'step-run/1/ship', path: 'ship', state: 'ready' } }],
};
const encoded = encodeProjectionCache(gateway, 'person/one', 'host/example-linux', 42, data, now);
assert.ok(encoded);
assert.equal(encoded.includes('must-not-persist'), false);
assert.equal(encoded.includes('also-secret'), false);
const hydrated = decodeProjectionCache(encoded, gateway, now + 1000);
assert.equal(hydrated?.actor, 'person/one');
assert.equal(hydrated?.storeIndex, 42);
assert.equal(hydrated?.data.sessions[0].id, 'session/1');
// Rows keep what st joined into them, so a cached Control or Chat tab draws without a work list.
assert.equal(hydrated?.data.missions[0].run_details[0].steps[0].path, 'build');
assert.equal(hydrated?.data.agents[0].next_work.path, 'ship');
assert.equal(hydrateProjectionForPairedDevice(encoded, gateway, false, now), null);
assert.equal(hydrateProjectionForPairedDevice(encoded, gateway, true, now)?.data.attention[0].title, 'Review');
assert.equal(decodeProjectionCache(encoded, 'https://other.invalid', now), null);
assert.equal(decodeProjectionCache(encoded, gateway, now + 8 * 24 * 60 * 60 * 1000), null);
assert.equal(decodeProjectionCache(encoded, gateway, now - 6 * 60 * 1000), null);
// A cache from before rows carried their steps is dropped rather than drawn without them.
assert.ok(encoded.includes('"version":4'));
assert.equal(decodeProjectionCache(encoded.replace('"version":4', '"version":3'), gateway, now), null);
assert.equal(decodeProjectionCache('{bad json', gateway, now), null);
assert.equal(decodeProjectionCache(encoded.replace('"kind":"session"', '"kind":"terminal-attachment"'), gateway, now), null);
assert.equal(offlinePresentation(false).title, 'Offline');
assert.match(offlinePresentation(false).detail, /No data is cached/);
assert.match(offlinePresentation(true).title, /showing last data/);

const excessive = { ...emptyData, sessions: Array.from({ length: 110 }, (_, i) => ({ id: `session/${i}`, kind: 'session' })) };
const bounded = decodeProjectionCache(encodeProjectionCache(gateway, 'person/one', 'host/example-linux', 42, excessive, now), gateway, now);
assert.equal(bounded?.data.sessions.length, 100);
assert.deepEqual(bounded?.truncated, ['sessions']);
const serverTruncated = decodeProjectionCache(encodeProjectionCache(gateway, 'person/one', 'host/example-linux', 42, emptyData, now, ['attention']), gateway, now);
assert.deepEqual(serverTruncated?.truncated, ['attention']);

// A direct Tailscale HTTP gateway gets the same bounded offline cache as HTTPS.
const tailnet = 'http://100.64.0.1:4102';
const tailnetCache = encodeProjectionCache(tailnet, 'person/one', 'host/example-linux', 42, data, now);
assert.ok(tailnetCache);
assert.equal(decodeProjectionCache(tailnetCache, tailnet, now)?.gateway, tailnet);
