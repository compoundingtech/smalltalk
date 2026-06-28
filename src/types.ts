// types.ts — public types for the embeddable @myobie/coord API.
//
// Branded primitives: Identity and Filename are both nominally `string` but
// carry phantom brands so a stray un-validated string doesn't compile. Use
// `asIdentity(s)` / `asFilename(s)` at the API boundary to validate + brand;
// `isIdentity(s)` / `isFilename(s)` are type-guard predicates.

import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';

import { validFilename, validIdentity } from './common.ts';
import {
  InvalidFilenameError,
  InvalidIdentityError,
  PeersConfigInvalidError,
} from './errors.ts';

// ─── Branded primitives ─────────────────────────────────────────────────

declare const IdentityBrand: unique symbol;
declare const FilenameBrand: unique symbol;

export type Identity = string & { readonly [IdentityBrand]: 'Identity' };
export type Filename = string & { readonly [FilenameBrand]: 'Filename' };

/**
 * Type-guard predicate. After `if (isIdentity(s))` the compiler narrows
 * `s` to {@link Identity}.
 */
export function isIdentity(s: string): s is Identity {
  return validIdentity(s);
}

/**
 * Validate `s` against the LAYOUT identity grammar and brand it. Throws
 * {@link InvalidIdentityError} on failure.
 */
export function asIdentity(s: string): Identity {
  if (!validIdentity(s)) {
    throw new InvalidIdentityError(s);
  }
  return s as Identity;
}

export function isFilename(s: string): s is Filename {
  return validFilename(s);
}

export function asFilename(s: string): Filename {
  if (!validFilename(s)) {
    throw new InvalidFilenameError(s);
  }
  return s as Filename;
}

// ─── State (re-exported convenience) ────────────────────────────────────

// `unknown` is a derived state surfaced when a status file's mtime is
// older than STATUS_STALE_MS in common.ts. Users cannot set it directly
// — see SETTABLE_STATES.
// `away` (brief-029) is the fifth settable state for "present but not
// actively engaged."
export type State =
  | 'offline'
  | 'available'
  | 'busy'
  | 'away'
  | 'dnd'
  | 'unknown';
export const STATES: readonly State[] = [
  'offline',
  'available',
  'busy',
  'away',
  'dnd',
  'unknown',
];

export function isState(s: string): s is State {
  return (STATES as readonly string[]).includes(s);
}

// ─── Message + MessageWithLocation ──────────────────────────────────────

export type Priority = 'low' | 'normal' | 'high';
export const PRIORITIES: readonly Priority[] = ['low', 'normal', 'high'];

/**
 * The portable subset of a message — everything that lives in the
 * frontmatter-or-body of a `<recipient>/inbox/<filename>.md` file.
 *
 * Per LAYOUT-004 `to:` and `ts:` are NOT in the file (`to` is the path,
 * `ts` is the filename's `<unix-ms>` prefix). Use {@link deriveTo} and
 * {@link deriveTs} when you need them.
 */
export interface Message {
  from: Identity;
  subject?: string;
  inReplyTo?: Filename;
  tags?: string[];
  priority?: Priority;
  body: string;
}

/** Locator + message: what `read` / `ls` / `thread` return. */
export interface MessageWithLocation {
  message: Message;
  identity: Identity;
  filename: Filename;
  folder: 'inbox' | 'archive';
}

/**
 * Derive the recipient identity from a message file path:
 * `<root>/<id>/inbox/<filename>` → `<id>`. Returns null if the path
 * doesn't match the LAYOUT shape.
 */
export function deriveTo(filename: Filename, identityFolder: string): Identity {
  // The caller supplies the `<id>` folder name directly — this is just a
  // narrowing helper that asserts it parses as an identity.
  if (!validIdentity(identityFolder)) {
    throw new InvalidIdentityError(identityFolder);
  }
  void filename; // unused but kept in signature for symmetry with deriveTs
  return identityFolder as Identity;
}

/**
 * Derive the canonical send-time `<unix-ms>` from a filename's prefix.
 * Throws if the filename doesn't match the LAYOUT grammar.
 */
export function deriveTs(filename: Filename): number {
  if (!validFilename(filename)) {
    throw new InvalidFilenameError(filename);
  }
  return Number(filename.slice(0, 13));
}

// ─── Resource + ResourceWithLocation ───────────────────────────────────

/**
 * A resource is an annotated URL an identity has chosen to surface to
 * peers. Lives at `<root>/<identity>/resources/<filename>.md` with the
 * url in frontmatter and an optional description in the body. Per
 * brief-009 item 5.
 *
 * `relation` is **very optional** — it's never inferred and the bare
 * URL stays first-class. Canonical (but non-enforced) values:
 * `owns` / `relates-to` / `depends-on`. Agents may invent their own
 * relation strings.
 */
export interface Resource {
  url: string;
  title?: string;
  tags?: string[];
  relation?: string;
  body: string;
}

/** Locator + resource: what resources.read / resources.ls return. */
export interface ResourceWithLocation {
  resource: Resource;
  identity: Identity;
  filename: Filename;
}

// ─── Watch event ────────────────────────────────────────────────────────

export interface WatchEvent {
  filename: Filename;
  identity: Identity;
  folder: 'inbox' | 'archive';
  /** Populated only when the watch was started with `withSubject: true`. */
  subject?: string;
}

// ─── Peer ───────────────────────────────────────────────────────────────

export type Peer = string;

export type PeerKind = 'local' | 'ssh' | 'alias';

export interface ParsedPeer {
  /** The original spec the caller passed in. */
  spec: Peer;
  /** Categorization of the spec. */
  kind: PeerKind;
  /** Resolved rsync root (with trailing slash). For `alias`, the
   * fully-resolved value after one indirection through `peers.yaml`. */
  resolved: string;
  /** For `alias` only: the alias name, separate from the resolved spec. */
  alias?: string;
}

/**
 * Parse a peer spec into a {@link ParsedPeer}. Mirrors the runtime
 * resolution behavior of `cmdSyncPush` etc.: `local:<path>` resolves to
 * `<path>/`, `host[:path]` to `<spec>/`, and a bare token is looked up
 * in `<configRoot>/peers.yaml` first (alias) before falling back to a
 * bare-hostname `<host>:.local/state/coord/`.
 *
 * Async because the alias lookup reads `peers.yaml`. The implementation
 * uses sync fs calls under the hood for simplicity; the Promise wrapper
 * matches the embeddable-API style.
 */
export async function parsePeer(
  spec: Peer,
  configRoot: string
): Promise<ParsedPeer> {
  if (spec.startsWith('local:')) {
    const path = spec.slice('local:'.length);
    if (path.length === 0) {
      throw new PeersConfigInvalidError(spec, 'local: peer requires a path');
    }
    return {
      spec,
      kind: 'local',
      resolved: `${stripTrailingSlash(path)}/`,
    };
  }
  if (spec.includes(':')) {
    return {
      spec,
      kind: 'ssh',
      resolved: `${stripTrailingSlash(spec)}/`,
    };
  }
  const aliased = lookupPeerAlias(spec, configRoot);
  if (aliased !== undefined) {
    const inner = await parsePeer(aliased, configRoot);
    return {
      spec,
      kind: 'alias',
      alias: spec,
      resolved: inner.resolved,
    };
  }
  return {
    spec,
    kind: 'ssh',
    resolved: `${spec}:.local/state/coord/`,
  };
}

function stripTrailingSlash(s: string): string {
  return s.endsWith('/') ? s.slice(0, -1) : s;
}

function lookupPeerAlias(
  name: string,
  configRoot: string
): string | undefined {
  const cfg = join(configRoot, 'peers.yaml');
  if (!existsSync(cfg)) return undefined;
  const text = readFileSync(cfg, 'utf8');
  for (const rawLine of text.split('\n')) {
    if (/^[ \t]*#/.test(rawLine)) continue;
    if (/^[ \t]*$/.test(rawLine)) continue;
    const idx = rawLine.indexOf(':');
    if (idx < 0) continue;
    const key = rawLine.slice(0, idx).trim();
    const value = rawLine.slice(idx + 1).trim();
    if (key === name && value.length > 0) return value;
  }
  return undefined;
}
