import assert from 'node:assert/strict';
import { parseFabricProofLink } from './fabricProof.ts';
import { normalizeGatewayUrl } from './gatewayUrl.ts';

const base = 'com.compoundingtech.smalltalk.starter://fabric-proof?node=' + 'ab'.repeat(32) + '&service=demo-client%2F0';
assert.deepEqual(parseFabricProofLink(base), { node: 'ab'.repeat(32), service: 'demo-client/0' });
assert.equal(parseFabricProofLink(base.replace('starter:', 'other:')), null);
assert.equal(parseFabricProofLink(base + '&id=pairing/demo'), null);
assert.equal(parseFabricProofLink(base.replace('demo-client%2F0', 'git%2Fdemo')), null);
assert.equal(parseFabricProofLink(base.replace('demo-client%2F0', '%0a')), null);
assert.equal(normalizeGatewayUrl('http://127.0.0.1:12345'), null, 'ordinary pairing must still reject typed loopback');
