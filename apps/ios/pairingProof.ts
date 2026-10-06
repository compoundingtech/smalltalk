// Match st3-client/device/grant_proof.rs. Crypto is supplied by Expo/CryptoKit in the app and
// node:crypto in tests; no gateway value can supply the independently trusted fingerprint.
import type { PairedSession } from '../../clients/typescript/st3-client';

export type ProofCrypto = {
  hash(bytes: Uint8Array): Promise<Uint8Array>;
  verify(key: string, text: string, signature: string): Promise<boolean>;
};
export const UNPINNED_WARNING = 'UNPINNED PAIRING: An active attacker can forge the identity-key chain. Copy the person-root fingerprint separately from the trusted machine to verify it.';
export const REPAIR_WARNING = 'This existing pairing has no trusted person-root pin. Copy the fingerprint separately from the trusted machine when re-pairing.';

function require(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}
function encoded(bytes: Uint8Array): string {
  return btoa(Array.from(bytes, byte => String.fromCharCode(byte)).join('')).replaceAll('+', '-').replaceAll('/', '_').replaceAll('=', '');
}
function decoded(text: string): Uint8Array {
  require(/^[A-Za-z0-9_-]+$/.test(text), 'Invalid public key or fingerprint encoding');
  let bytes: Uint8Array;
  try {
    const raw = text.replaceAll('-', '+').replaceAll('_', '/');
    bytes = Uint8Array.from(atob(raw + '='.repeat((4 - raw.length % 4) % 4)), ch => ch.charCodeAt(0));
  } catch { throw new Error('Invalid public key or fingerprint encoding'); }
  require(encoded(bytes) === text, 'Invalid public key or fingerprint encoding');
  return bytes;
}
export function validatePairingTrust(fingerprint: string, unpinned: boolean): string | undefined {
  const pin = fingerprint.trim();
  require(!(pin && unpinned), 'Choose a fingerprint or the explicit unpinned override, never both.');
  require(pin || unpinned, 'Enter the person-root fingerprint from the trusted machine, or explicitly choose unpinned pairing.');
  if (pin) {
    require(pin.startsWith('sha256:') && decoded(pin.slice(7)).length === 32, 'Use the full sha256: fingerprint printed on the trusted machine.');
    return pin;
  }
}

// ciborium serializes canonical JSON maps in Rust's lexical key order, with shortest integer
// lengths. Grant fields contain strings, nulls, booleans and integers. Unsupported numbers fail
// closed rather than rounding content hashes. Bound size and depth before encoding untrusted data.
function cbor(value: unknown): Uint8Array {
  const out: number[] = [];
  const utf8 = new TextEncoder();
  function head(major: number, count: number) {
    if (count < 24) out.push(major * 32 + count);
    else {
      const size = count <= 0xff ? 1 : count <= 0xffff ? 2 : count <= 0xffffffff ? 4 : 8;
      out.push(major * 32 + ({ 1: 24, 2: 25, 4: 26, 8: 27 } as Record<number, number>)[size]);
      let n = BigInt(count);
      const bytes = Array<number>(size);
      for (let at = size - 1; at >= 0; at--) { bytes[at] = Number(n & 255n); n >>= 8n; }
      out.push(...bytes);
    }
  }
  function write(item: unknown, depth: number) {
    require(depth <= 32, 'Enrollment proof is too deeply nested');
    if (item === null) out.push(0xf6);
    else if (typeof item === 'boolean') out.push(item ? 0xf5 : 0xf4);
    else if (typeof item === 'number') {
      require(Number.isSafeInteger(item), 'Unsupported enrollment proof number');
      head(item < 0 ? 1 : 0, item < 0 ? -1 - item : item);
    } else if (typeof item === 'string') {
      require(!Array.from(item).some(ch => ch.codePointAt(0)! >= 0xd800 && ch.codePointAt(0)! <= 0xdfff), 'Invalid enrollment proof text');
      const bytes = utf8.encode(item); head(3, bytes.length); out.push(...bytes);
    } else if (Array.isArray(item)) {
      head(4, item.length); for (const entry of item) write(entry, depth + 1);
    } else {
      require(typeof item === 'object' && item !== undefined, 'Incomplete enrollment proof');
      const fields = Object.entries(item).sort(([a], [b]) => {
        const left = utf8.encode(a), right = utf8.encode(b);
        for (let i = 0; i < Math.min(left.length, right.length); i++) if (left[i] !== right[i]) return left[i] - right[i];
        return left.length - right.length;
      });
      head(5, fields.length);
      for (const [key, entry] of fields) { write(key, depth + 1); write(entry, depth + 1); }
    }
  }
  write(value, 0);
  return Uint8Array.from(out);
}

type Grant = {
  id: string; batch_id: string; subject: string; kind: string; origin: string; actor: string | null;
  body: Record<string, unknown> & { fields: Record<string, unknown> }; predecessors: string[];
  signature: { signer: string; key: string; chain: string[]; nonce: string; signed_at_unix_ms: number; signature: string; on_behalf?: string | null; format?: string | null; signed_fields?: string[] };
};
const strings = (value: unknown): value is string[] => Array.isArray(value) && value.every(item => typeof item === 'string');
const object = (value: unknown): value is Record<string, unknown> => !!value && typeof value === 'object' && !Array.isArray(value);

async function grantProof(value: unknown, id: string | undefined, person: string, crypto: ProofCrypto): Promise<Grant> {
  require(value && new TextEncoder().encode(JSON.stringify(value)).length <= 64 * 1024, 'Missing or oversized enrollment proof; upgrade the member.');
  require(object(value) && ['id', 'batch_id', 'subject', 'kind', 'origin'].every(field => typeof value[field] === 'string')
    && (value.actor == null || typeof value.actor === 'string') && strings(value.predecessors) && object(value.body)
    && object(value.body.fields) && object(value.signature), 'Incomplete enrollment grant proof');
  const sig = value.signature;
  require(['signer', 'key', 'nonce', 'signature'].every(field => typeof sig[field] === 'string') && strings(sig.chain)
    && Number.isSafeInteger(sig.signed_at_unix_ms) && (sig.signed_at_unix_ms as number) >= 0
    && sig.on_behalf == null && sig.format == null && (sig.signed_fields === undefined || (strings(sig.signed_fields) && sig.signed_fields.length === 0)), 'Unsupported enrollment grant signature');
  const grant = value as unknown as Grant;
  require(id === undefined || grant.id === id, 'Enrollment proof does not match the returned chain');
  require(grant.kind === 'principal.key-granted' && grant.subject === person, 'Enrollment proof is not a grant for the paired person');
  const hex = async (item: unknown) => Array.from(await crypto.hash(cbor(item)), b => b.toString(16).padStart(2, '0')).join('');
  const actor = grant.actor ?? null;
  require(await hex([grant.batch_id, grant.subject, grant.kind, grant.origin, actor, grant.body, grant.predecessors]) === grant.id, 'Enrollment grant content does not match its claim ID');
  const content = await hex(['smallclaims.claim-content.v1', grant.subject, grant.kind, actor, grant.body]);
  const text = `smallclaims-claim-v1\n${content}\n${sig.signer}\n\n${sig.key}\n${(sig.chain as string[]).join(',')}\n${sig.nonce}\n${sig.signed_at_unix_ms}`;
  require(await crypto.verify(grant.signature.key, text, grant.signature.signature), 'Enrollment grant signature does not verify');
  return grant;
}

async function rootProof(root: Grant, fingerprint: string | undefined, crypto: ProofCrypto) {
  const fields = root.body.fields, sig = root.signature;
  require(fields.role === 'root' && typeof fields.issuer === 'string' && fields.issuer.startsWith('host/') && fields.issuer.length > 5
    && fields.issuer === sig.signer && fields.issuer_key === sig.key && sig.chain.length === 0, 'Returned person root grant is not issued by the node authority');
  require(typeof fields.key === 'string', 'Person root grant has no key');
  const p256 = fields.key.startsWith('p256:'), raw = decoded(p256 ? fields.key.slice(5) : fields.key);
  require(p256 ? raw.length === 65 && raw[0] === 4 : raw.length === 32, 'Invalid person-root public key');
  const actual = `sha256:${encoded(await crypto.hash(new TextEncoder().encode(fields.key)))}`;
  require(!fingerprint || fingerprint === actual, 'Person-root fingerprint mismatch. Do not trust this response; obtain the fingerprint separately from the trusted machine.');
}

/** Check enrollment before saving any credential. A fingerprint must be validated before POST. */
export async function verifyPairing(session: PairedSession, key: string, fingerprint: string | undefined, crypto: ProofCrypto): Promise<void> {
  require(typeof session.person_id === 'string' && /^person\/[^/]+$/.test(session.person_id)
    && typeof session.credential === 'string' && session.credential.length >= 32 && strings(session.scopes), 'Invalid paired session');
  const chain = session.device_key_chain ?? [], proofs = session.device_key_proofs ?? [];
  require(strings(chain) && Array.isArray(proofs), 'Incomplete enrollment grants');
  const signs = session.scopes.includes('control.messages');
  if (signs) {
    require(chain.length === 2 && proofs.length === 2, 'Member did not return verifiable device and root grants; upgrade the member.');
    const device = await grantProof(proofs[0], chain[0], session.person_id, crypto);
    const root = await grantProof(proofs[1], chain[1], session.person_id, crypto);
    const fields = device.body.fields;
    require(fields.key === key && fields.role === 'device' && device.actor === session.person_id && fields.issuer === session.person_id
      && device.signature.signer === session.person_id && fields.issuer_key === root.body.fields.key && device.signature.key === root.body.fields.key
      && device.signature.chain.length === 1 && device.signature.chain[0] === chain[1], 'Returned device grant does not bind our public key to the paired person’s root');
    await rootProof(root, fingerprint, crypto);
  } else {
    require(chain.length === 0 && proofs.length === 0, 'Read-only pairing unexpectedly enrolled a signing key');
    if (session.person_root_key_proof) await rootProof(await grantProof(session.person_root_key_proof, undefined, session.person_id, crypto), fingerprint, crypto);
    else require(!fingerprint, 'Member did not return a person-root proof for read-only pairing; upgrade it or explicitly choose unpinned pairing.');
  }
}
