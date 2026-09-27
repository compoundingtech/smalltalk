import assert from 'node:assert/strict';
import { gatewayFetch } from './gatewayFetch.ts';

// A proxy failure (for example Tailscale Serve's empty 502) must read as a gateway failure,
// not as "JSON Parse error: Unexpected end of input".
const empty502 = gatewayFetch(async () => new Response('', { status: 502 }));
await assert.rejects(empty502('https://gateway.example/v1/client/work?limit=30'), /gateway returned HTTP 502 with no st3 response for \/v1\/client\/work/);
const html = gatewayFetch(async () => new Response('<html>Bad Gateway</html>', { status: 502, headers: { 'content-type': 'text/html' } }));
await assert.rejects(html('https://gateway.example/v1/client/agents'), /gateway returned HTTP 502 with no st3 response/);

// st3 JSON responses, including st3 error envelopes, pass through untouched.
const body = JSON.stringify({ api_version: 'st3.client.v0', value: { ok: true } });
const ok = await gatewayFetch(async () => new Response(body, { status: 200, headers: { 'content-type': 'application/json' } }))('https://gateway.example/v1/client/capabilities');
assert.deepEqual(await ok.json(), JSON.parse(body));
const denied = await gatewayFetch(async () => new Response('{"api_version":"st3.client.v0","code":"forbidden"}', { status: 403, headers: { 'content-type': 'application/json' } }))('https://gateway.example/v1/client/devices');
assert.equal(denied.status, 403);
