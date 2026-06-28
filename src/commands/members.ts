// commands/members.ts — enumerate identities under $COORD_ROOT.
//
// "Roster" verb: walks `<root>/<*>` and reports every identity-shaped
// sub-folder (one with at least one of inbox/ or archive/). Plain
// filesystem read, no resolveIdentity — does not auto-create or mutate
// anything for the identities it walks.
//
//   coord members                  # text, sorted alphabetically
//   coord members --status STATE   # filter to a single status
//   coord members --json           # machine-readable
//   coord members --json --enrich  # + lastActivity, inbox count
//
// Reserved as an identity name in common.ts so no one can collide
// with the verb's namespace.

import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';

import type { CliContext } from '../cli-context.ts';
import {
  archiveDir,
  inboxDir,
  RESERVED_NAMES,
  statusPath,
  validFilename,
  validIdentity,
} from '../common.ts';
import { type State } from '../types.ts';

import { readIdentityStatus } from './status.ts';

export interface MemberSummary {
  identity: string;
  status: State;
  name: string | null;
}

export interface MemberSummaryEnriched extends MemberSummary {
  /** Newest mtime across inbox/archive/status, or null if
   * nothing at all under the identity has been touched. */
  lastActivity: number | null;
  /** Count of valid-grammar files in <id>/inbox/ (mirrors `coord ls --count`). */
  inbox: number;
}

export interface MembersInput {
  status?: string | undefined;
  enrich?: boolean | undefined;
  coordRoot: string;
}

export interface MembersResult<TEnriched extends boolean = false> {
  items: TEnriched extends true ? MemberSummaryEnriched[] : MemberSummary[];
}

// ─── Core ───────────────────────────────────────────────────────────────

export interface GetMembersOpts {
  /** Filter to identities whose effective status matches. */
  status?: string | undefined;
  /** When true, return MemberSummaryEnriched[]; otherwise MemberSummary[]. */
  enrich?: boolean | undefined;
}

/**
 * Pure library-shaped enumeration. Same computation as {@link cmdMembers}
 * but takes positional `root` and returns the bare array (no `{items}`
 * envelope) — what `coord.members(...)` exposes to embedders per
 * brief-028.
 *
 * Read-only: walks `<root>/*` and consults each identity's status / name
 * / inbox. No writes.
 */
export function getMembers(
  root: string,
  opts: GetMembersOpts = {}
): MemberSummary[] | MemberSummaryEnriched[] {
  const ids = listIdentities(root);
  const base: MemberSummary[] = ids.map((id) => ({
    identity: id,
    status: readIdentityStatus(id, root),
    name: readNameFile(id, root),
  }));
  const filtered =
    opts.status !== undefined && opts.status !== ''
      ? base.filter((m) => m.status === opts.status)
      : base;
  if (opts.enrich !== true) {
    return filtered;
  }
  const enriched: MemberSummaryEnriched[] = filtered.map((m) => ({
    ...m,
    lastActivity: computeLastActivity(m.identity, root),
    inbox: computeInboxCount(m.identity, root),
  }));
  return enriched;
}

/**
 * CLI-shaped wrapper retained for backward compatibility: keeps the
 * `{items}` envelope return shape and the input-object signature that
 * existing callers (the MCP coord_members tool, overview.ts, the
 * existing test suite) depend on. Delegates to {@link getMembers}; do
 * not duplicate logic here.
 */
export function cmdMembers(
  input: MembersInput
): MembersResult<false> | MembersResult<true> {
  const items = getMembers(input.coordRoot, {
    ...(input.status !== undefined && { status: input.status }),
    ...(input.enrich !== undefined && { enrich: input.enrich }),
  });
  if (input.enrich === true) {
    return { items: items as MemberSummaryEnriched[] };
  }
  return { items: items as MemberSummary[] };
}

// ─── Helpers (exported for overview.ts) ─────────────────────────────────

/**
 * Walk `<root>/*` and return identity-shaped subfolders.
 *
 * Filters:
 *   - skip dotfiles (defensive; nothing in coord uses them today)
 *   - skip non-directories
 *   - skip reserved names (defensive)
 *   - keep only names where validIdentity(name) holds AND at least
 *     one of `<name>/inbox`, `<name>/archive` exists
 *
 * Returns alphabetically sorted.
 */
export function listIdentities(root: string): string[] {
  let entries: string[];
  try {
    entries = readdirSync(root);
  } catch {
    return [];
  }
  const out: string[] = [];
  for (const name of entries) {
    if (name.startsWith('.')) continue;
    if (RESERVED_NAMES.includes(name)) continue;
    if (!validIdentity(name)) continue;
    const dir = join(root, name);
    let st: ReturnType<typeof statSync>;
    try {
      st = statSync(dir);
    } catch {
      continue;
    }
    if (!st.isDirectory()) continue;
    if (
      !isDir(inboxDir(name, root)) &&
      !isDir(archiveDir(name, root))
    ) {
      continue;
    }
    out.push(name);
  }
  return out.sort();
}

function readNameFile(id: string, root: string): string | null {
  const path = join(root, id, 'name');
  if (!existsSync(path)) return null;
  try {
    const raw = readFileSync(path, 'utf8');
    const trimmed = raw.split('\n')[0]?.trim() ?? '';
    return trimmed.length > 0 ? trimmed : null;
  } catch {
    return null;
  }
}

/** Newest mtime across inbox/archive/status under <id>/. */
export function computeLastActivity(
  identity: string,
  root: string
): number | null {
  let newest: number | null = null;
  const consider = (path: string): void => {
    let st: ReturnType<typeof statSync>;
    try {
      st = statSync(path);
    } catch {
      return;
    }
    if (newest === null || st.mtimeMs > newest) newest = st.mtimeMs;
  };

  // Walk one level inside each folder; we don't recurse into nested
  // structure that doesn't exist by convention.
  for (const dir of [
    inboxDir(identity, root),
    archiveDir(identity, root),
  ]) {
    if (!isDir(dir)) continue;
    let names: string[];
    try {
      names = readdirSync(dir);
    } catch {
      continue;
    }
    for (const n of names) consider(join(dir, n));
  }
  // Status file (a regular file, not a dir).
  const sp = statusPath(identity, root);
  if (existsSync(sp)) consider(sp);
  return newest;
}

export function computeInboxCount(identity: string, root: string): number {
  const dir = inboxDir(identity, root);
  if (!isDir(dir)) return 0;
  let names: string[];
  try {
    names = readdirSync(dir);
  } catch {
    return 0;
  }
  return names.filter((n) => validFilename(n)).length;
}

function isDir(p: string): boolean {
  try {
    return statSync(p).isDirectory();
  } catch {
    return false;
  }
}

// ─── CLI wrapper ────────────────────────────────────────────────────────

const MEMBERS_HELP =
  'usage: coord members [--status STATE] [--json [--enrich]]\n\n' +
  '  Enumerate every identity under $COORD_ROOT — i.e. any\n' +
  '  sub-folder with at least one of inbox/ or archive/.\n' +
  '  Sorted alphabetically. Plain read; does not mutate state.\n';

export function cmdMembersCli(
  args: readonly string[],
  ctx: CliContext
): number {
  let status: string | undefined;
  let json = false;
  let enrich = false;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '--status':
        status = args[++i];
        break;
      case '--json':
        json = true;
        break;
      case '--enrich':
        enrich = true;
        break;
      case '-h':
      case '--help':
        ctx.stderr(MEMBERS_HELP);
        return 0;
      default:
        throw new Error(`unknown flag: ${a}`);
    }
  }
  if (enrich && !json) {
    throw new Error('--enrich requires --json');
  }
  const r = cmdMembers({
    ...(status !== undefined && { status }),
    enrich,
    coordRoot: ctx.coordRoot,
  });
  if (json) {
    ctx.stdout(`${JSON.stringify(r.items)}\n`);
    return 0;
  }
  for (const m of r.items) {
    ctx.stdout(`${m.identity}\t${m.status}\t${m.name ?? ''}\n`);
  }
  return 0;
}
