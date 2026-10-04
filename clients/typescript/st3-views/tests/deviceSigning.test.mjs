import assert from 'node:assert/strict';
import { createHash, createPrivateKey, createPublicKey, sign, verify } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { base64url, messageSubject, signatureParameter, signatureRefusal, signedBytes } from '@smalltalk/st3-views/deviceSigning';

// The shared vectors every client checks (docs/st3/device-signing.md).
const vectors = JSON.parse(readFileSync(new URL('../../../../fixtures/clients/device-signing-v1.json', import.meta.url), 'utf8'));
assert.equal(vectors.format, 'fields-v1');
const privateKey = createPrivateKey({ key: Buffer.from(vectors.key_pkcs8_hex, 'hex'), format: 'der', type: 'pkcs8' });
const publicKey = createPublicKey(privateKey);
// The key is written `p256:` and the base64url of the uncompressed SEC1 point, as CryptoKit's
// x963Representation gives it.
const point = publicKey.export({ format: 'der', type: 'spki' }).subarray(-65);
const key = `p256:${base64url(new Uint8Array(point))}`;

for (const vector of vectors.cases) {
  assert.equal(key, vector.key, vector.name);
  const subject = messageSubject(createHash('sha256').update(vector.idempotency_key).digest('hex'));
  assert.equal(subject, vector.subject, vector.name);
  const message = { subject, signer: vector.signer, fields: vector.fields, key: vector.key, chain: vector.chain, nonce: vector.nonce, signedAt: vector.signed_at_unix_ms };
  const bytes = signedBytes(message);
  assert.equal(bytes, vector.signed_bytes, vector.name);
  // The vector's signature verifies over those bytes as a raw 64-byte r||s (CryptoKit's
  // rawRepresentation).
  const vectorSignature = Buffer.from(vector.signature, 'base64url');
  assert.equal(vectorSignature.length, 64);
  assert.ok(verify('sha256', Buffer.from(bytes, 'utf8'), { key: publicKey, dsaEncoding: 'ieee-p1363' }, vectorSignature), vector.name);
  // A fresh signature (ECDSA is randomized) made the way the phone makes one also verifies, and
  // the parameter names exactly the fields the daemon expects.
  const fresh = base64url(new Uint8Array(sign('sha256', Buffer.from(bytes, 'utf8'), { key: privateKey, dsaEncoding: 'ieee-p1363' })));
  assert.ok(verify('sha256', Buffer.from(bytes, 'utf8'), { key: publicKey, dsaEncoding: 'ieee-p1363' }, Buffer.from(fresh, 'base64url')));
  const parameter = signatureParameter(message, fresh);
  assert.deepEqual(parameter.signed_fields, vector.signed_fields);
  assert.deepEqual({ ...parameter, signature: undefined }, { signer: vector.signer, key: vector.key, chain: vector.chain, nonce: vector.nonce, signed_at_unix_ms: vector.signed_at_unix_ms, signature: undefined, format: 'fields-v1', signed_fields: vector.signed_fields });
}

// base64url without padding, for every remainder.
for (const length of [0, 1, 2, 3, 16, 64, 65]) {
  const bytes = new Uint8Array(Array.from({ length }, (_, index) => (index * 37 + 250) % 256));
  assert.equal(base64url(bytes), Buffer.from(bytes).toString('base64url'));
}

// Refusals are said in plain words; other codes are left to the general wording.
assert.match(signatureRefusal('device-signature-stale'), /clock/);
assert.match(signatureRefusal('device-key-not-enrolled', 'phone'), /pair the phone again/);
assert.equal(signatureRefusal('stale-fence'), null);

assert.match(signatureRefusal('device-key-not-enrolled'), /pair the device again/);
