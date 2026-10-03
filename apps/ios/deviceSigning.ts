// Device signing, version 1 (docs/st3/device-signing.md): the bytes a paired phone signs for each
// message it sends, and the `signature` parameter that carries the result. The key itself lives in
// the native module (modules/st-device-key); everything here is plain data, checked by
// deviceSigning.test.mjs against fixtures/clients/device-signing-v1.json.
import type { DeviceSignature } from '../../clients/typescript/st3-client';

export const SIGNED_FIELDS = ['content', 'from', 'in_reply_to', 'session_id', 'tags', 'title', 'to'] as const;

/** A message's fields as the daemon stores them: absent text fields are null, tags `[]`. */
export type SignedFields = {
  content: string;
  from: string;
  in_reply_to: string | null;
  session_id: string | null;
  tags: string[];
  title: string | null;
  to: string;
};

/** What a paired phone keeps beside its key: who it signs as and the grants that enroll the key. */
export type DeviceKey = { key: string; storage: 'secure-enclave' | 'software'; person: string; chain: string[] };

/**
 * A field's value in RFC 8785 (JCS) form. The signed values are strings, null and arrays of
 * strings, for which JSON.stringify is exactly JCS: compact, `/` and non-ASCII not escaped.
 */
export function canonical(value: string | null | string[]): string {
  return JSON.stringify(value);
}

/** The message's subject, from the hex SHA-256 of its idempotency key, as the daemon names it. */
export function messageSubject(idempotencyKeySha256Hex: string): string {
  return `message/${idempotencyKeySha256Hex.slice(0, 16)}`;
}

export type Unsigned = {
  subject: string;
  signer: string;
  fields: SignedFields;
  key: string;
  chain: string[];
  nonce: string;
  signedAt: number;
};

/** The UTF-8 lines the device signs, joined by `\n`, with no trailing newline. */
export function signedBytes(message: Unsigned): string {
  return [
    'smallclaims-claim-fields-v1',
    message.subject,
    'message.sent',
    message.signer,
    ...SIGNED_FIELDS.map(field => `${field}=${canonical(message.fields[field])}`),
    message.signer,
    '',
    message.key,
    message.chain.join(','),
    message.nonce,
    String(message.signedAt),
  ].join('\n');
}

/** `parameters.signature` for message.send, from the signed message and its signature. */
export function signatureParameter(message: Unsigned, signature: string): DeviceSignature {
  return {
    signer: message.signer,
    key: message.key,
    chain: message.chain,
    nonce: message.nonce,
    signed_at_unix_ms: message.signedAt,
    signature,
    format: 'fields-v1',
    signed_fields: [...SIGNED_FIELDS],
  };
}

/** Base64url without padding. */
export function base64url(bytes: Uint8Array): string {
  const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_';
  let out = '';
  for (let index = 0; index < bytes.length; index += 3) {
    const chunk = (bytes[index] << 16) | ((bytes[index + 1] ?? 0) << 8) | (bytes[index + 2] ?? 0);
    const left = bytes.length - index;
    out += alphabet[(chunk >> 18) & 63] + alphabet[(chunk >> 12) & 63];
    if (left > 1) out += alphabet[(chunk >> 6) & 63];
    if (left > 2) out += alphabet[chunk & 63];
  }
  return out;
}

/** A refused signature, in words a person reads (and what to do about it). */
export function signatureRefusal(code: string): string | null {
  switch (code) {
    case 'device-signature-replayed': return 'st already has a message signed with this one-time number; send it again as a new message';
    case 'device-signature-noncanonical': return 'the recipient must be named in full (agent/… or person/…) for a signed message';
    case 'device-signature-stale': return "this phone's clock is more than 15 minutes off; set the time automatically and send again";
    case 'device-key-not-enrolled': return "st does not know this phone's signing key; pair the phone again";
    case 'device-signature-invalid': return "st could not verify this phone's signature on the message";
    case 'device-signature-signer': return 'this phone signs as a different person than it is paired as; pair it again';
    case 'device-signature-format': return 'st expects a different signature format; this app or st needs an update';
    default: return null;
  }
}
