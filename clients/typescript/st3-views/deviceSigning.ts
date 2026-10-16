// Device signing, version 1 (docs/st3/device-signing.md): the bytes a paired device signs for each
// message it sends, and the `signature` parameter that carries the result. The key itself lives in
// the caller's platform key store; everything here is plain data, checked by
// tests/deviceSigning.test.mjs against fixtures/clients/device-signing-v1.json.
import type { DeviceSignature } from '@smalltalk/st3-client';

export const SIGNED_FIELDS = ['content', 'from', 'in_reply_to', 'session_id', 'tags', 'title', 'to'] as const;

/**
 * What a device signs on a message past `INLINE_MAX_BYTES` (docs/st3/long-messages.md): the claim
 * keeps only a preview, so the signature covers the preview, the hash and the length of the whole
 * text. Sorted, as the daemon compares them.
 */
export const SIGNED_LONG_FIELDS = ['body_bytes', 'body_sha256', 'content', 'from', 'in_reply_to', 'session_id', 'tags', 'title', 'to'] as const;

/** The longest message, in UTF-8 bytes, whose text stays whole in the claim. */
export const INLINE_MAX_BYTES = 8192;
const PREVIEW_BYTES = 1024;

/** A message's fields as the daemon stores them: absent text fields are null, tags `[]`. */
export type SignedFields = {
  /** Past INLINE_MAX_BYTES: the whole text's length in UTF-8 bytes (with body_sha256). */
  body_bytes?: number;
  /** Past INLINE_MAX_BYTES: the lowercase hex SHA-256 of the whole text's UTF-8 bytes. */
  body_sha256?: string;
  content: string;
  from: string;
  in_reply_to: string | null;
  session_id: string | null;
  tags: string[];
  title: string | null;
  to: string;
};

/** What a paired device keeps beside its key: who it signs as and the grants that enroll the key. */
export type DeviceKey = { key: string; storage: 'secure-enclave' | 'software'; person: string; chain: string[] };

/**
 * A field's value in RFC 8785 (JCS) form. The signed values are strings, null and arrays of
 * strings, for which JSON.stringify is exactly JCS: compact, `/` and non-ASCII not escaped.
 */
export function canonical(value: string | number | null | string[]): string {
  return JSON.stringify(value);
}

/** A string's length in UTF-8 bytes; a lone surrogate counts as the three bytes of U+FFFD. */
export function utf8Bytes(text: string): number {
  let bytes = 0;
  for (const character of text) {
    const code = character.codePointAt(0)!;
    bytes += code < 0x80 ? 1 : code < 0x800 ? 2 : code < 0x10000 ? 3 : 4;
  }
  return bytes;
}

/** Whether the text is past what the claim keeps whole, so a device signs its preview and hash. */
export function isLongMessage(content: string): boolean {
  return utf8Bytes(content) > INLINE_MAX_BYTES;
}

// Rust's `str::trim_end`: Unicode White_Space (not JavaScript's `\s`, which has U+FEFF and lacks U+0085).
const TRAILING_WHITE_SPACE = /[\t-\r \u0085\u00a0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000]+$/u;

/**
 * The start of a long message as the daemon keeps it in the claim: the first 1,024 UTF-8 bytes cut
 * at a character, trailing white space trimmed, then `…`. A device signs exactly this.
 */
export function longPreview(text: string): string {
  let bytes = 0;
  let end = 0;
  for (const character of text) {
    const code = character.codePointAt(0)!;
    const size = code < 0x80 ? 1 : code < 0x800 ? 2 : code < 0x10000 ? 3 : 4;
    if (bytes + size > PREVIEW_BYTES) break;
    bytes += size;
    end += character.length;
  }
  return `${text.slice(0, end).replace(TRAILING_WHITE_SPACE, '')}…`;
}

/**
 * The fields a device signs for a message with this `content`. Up to INLINE_MAX_BYTES that is the
 * text itself; past it, the preview with the hash and length of the whole, and `sha256Hex` (the hex
 * SHA-256 of the UTF-8 text, from the platform's hash) is required.
 */
export function signedMessageFields(others: Omit<SignedFields, 'content' | 'body_bytes' | 'body_sha256'>, content: string, sha256Hex?: string): SignedFields {
  if (!isLongMessage(content)) return { ...others, content };
  if (!sha256Hex || !/^[0-9a-f]{64}$/.test(sha256Hex)) throw new Error('a message this long needs the hash of its text to be signed');
  return { ...others, content: longPreview(content), body_bytes: utf8Bytes(content), body_sha256: sha256Hex };
}

/** The signed field names for these fields: the long form when they carry a body hash. */
export function signedFieldNames(fields: SignedFields): readonly string[] {
  return fields.body_sha256 === undefined ? SIGNED_FIELDS : SIGNED_LONG_FIELDS;
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
    ...signedFieldNames(message.fields).map(field => `${field}=${canonical((message.fields as Record<string, string | number | null | string[]>)[field])}`),
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
    signed_fields: [...signedFieldNames(message.fields)],
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
export function signatureRefusal(code: string, device = 'device'): string | null {
  switch (code) {
    case 'device-signature-replayed': return 'st already has a message signed with this one-time number; send it again as a new message';
    case 'device-signature-noncanonical': return 'the recipient must be named in full (agent/… or person/…) for a signed message';
    case 'device-signature-stale': return `this ${device}'s clock is more than 15 minutes off; set the time automatically and send again`;
    case 'device-key-not-enrolled': return `st does not know this ${device}'s signing key; pair the ${device} again`;
    case 'device-signature-invalid': return `st could not verify this ${device}'s signature on the message`;
    case 'device-signature-signer': return `this ${device} signs as a different person than it is paired as; pair it again`;
    case 'device-signature-format': return 'st expects a different signature format; this app or st needs an update';
    default: return null;
  }
}
