import assert from 'node:assert/strict';
import { createHash, createPrivateKey, createPublicKey, sign, verify } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { base64url, INLINE_MAX_BYTES, isLongMessage, longPreview, messageSubject, signatureParameter, signatureRefusal, signedBytes, signedMessageFields, utf8Bytes } from '@smalltalk/st3-views/deviceSigning';

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
  // What the person sent: a long case names it as a unit and a count, and its `fields.content` is
  // only the preview that is signed. The phone must derive those fields from the text.
  const text = vector.body ? vector.body.unit.repeat(vector.body.repeat) : vector.fields.content;
  const { body_bytes, body_sha256, content, ...others } = vector.fields;
  const fields = signedMessageFields(others, text, isLongMessage(text) ? createHash('sha256').update(text, 'utf8').digest('hex') : undefined);
  assert.deepEqual(fields, vector.fields, vector.name);
  assert.equal(isLongMessage(text), vector.name === 'long', vector.name);
  const message = { subject, signer: vector.signer, fields, key: vector.key, chain: vector.chain, nonce: vector.nonce, signedAt: vector.signed_at_unix_ms };
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

// A long message signs its preview, the hash and the length of the whole text.
assert.ok(vectors.cases.some(vector => vector.name === 'long'), 'the shared vectors have a long case');
assert.equal(INLINE_MAX_BYTES, 8192);
const exactly = 'a'.repeat(INLINE_MAX_BYTES);
assert.equal(isLongMessage(exactly), false);
assert.equal(isLongMessage(`${exactly}a`), true);
// Bytes, not characters: 3-byte and 4-byte characters push a short string past the limit.
assert.equal(utf8Bytes('é'), 2);
assert.equal(utf8Bytes('€'), 3);
assert.equal(utf8Bytes('🎉'), 4);
assert.equal(isLongMessage('€'.repeat(2731)), true);
assert.equal(isLongMessage('€'.repeat(2730)), false);
// The preview is cut at a character within 1,024 bytes, trailing white space trimmed, then `…`.
assert.equal(longPreview('a'.repeat(5000)), `${'a'.repeat(1024)}…`);
assert.equal(longPreview(`${'a'.repeat(1020)} \n\t ${'b'.repeat(100)}`), `${'a'.repeat(1020)}…`);
assert.equal(longPreview(`${'a'.repeat(1023)}🎉tail`), `${'a'.repeat(1023)}…`, 'a character is never cut');
assert.equal(longPreview(`${'a'.repeat(1022)}€x`), `${'a'.repeat(1022)}…`);
// Rust trims U+0085 and U+3000 but not U+FEFF.
assert.equal(longPreview(`${'a'.repeat(1019)}\u0085\u3000${'b'.repeat(10)}`), `${'a'.repeat(1019)}…`, 'U+0085 and U+3000 are trimmed, as Rust trims them');
assert.equal(longPreview(`${'a'.repeat(1016)}\ufeff\ufeff€`), `${'a'.repeat(1016)}\ufeff\ufeff…`, 'U+FEFF is not white space to Rust');
// A long message cannot be signed without the hash of its text.
assert.throws(() => signedMessageFields({ from: 'person/avery', in_reply_to: null, session_id: null, tags: [], title: null, to: 'agent/alder' }, 'a'.repeat(9000)), /hash/);
assert.throws(() => signedMessageFields({ from: 'person/avery', in_reply_to: null, session_id: null, tags: [], title: null, to: 'agent/alder' }, 'a'.repeat(9000), 'nothex'), /hash/);
