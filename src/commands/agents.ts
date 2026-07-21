// commands/agents.ts — enumerate agents under $ST_ROOT.
//
// "Roster" verb: walks `<root>/<*>` and reports every agent-shaped
// sub-folder (one with at least one of inbox/ or archive/). Plain
// filesystem read, no resolveAgent — does not auto-create or mutate
// anything for the agents it walks.
//
//   st agents                  # text, sorted alphabetically
//   st agents --status STATE   # filter to a single status
//   st agents --json           # machine-readable
//   st agents --json --enrich  # + lastActivity, inbox count
//
// brief-009 item 3 (rename): `members` is the deprecated alias of this
// verb. Both spellings work; CLI + MCP both dual-register. The type
// names below — MemberSummary / MemberSummaryEnriched / MembersInput /
// MembersResult / GetMembersOpts — keep the legacy "Members" prefix
// for back-compat re-exports. New code should prefer the Agent*
// aliases declared at the bottom of this file.

import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';

import { invokedName, type CliContext } from '../cli-context.ts';
import {
  archiveDir,
  inboxDir,
  RESERVED_NAMES,
  statusPath,
  validAgent,
  validFilename,
} from '../common.ts';
import { type State } from '../types.ts';

import { readIdentityStatus } from './status.ts';

export interface AgentSummary {
  /** The agent's name. Field kept as `identity` for back-compat with
   *  embedder destructures; will rename to `agent` in a follow-up. */
  identity: string;
  status: State;
  name: string | null;
}

export interface AgentSummaryEnriched extends AgentSummary {
  /** Newest mtime across inbox/archive/status, or null if nothing at
   *  all under the agent has been touched. */
  lastActivity: number | null;
  /** Count of valid-grammar files in <agent>/inbox/ (mirrors `st ls
   *  --count`). */
  inbox: number;
}

export interface AgentsInput {
  status?: string | undefined;
  enrich?: boolean | undefined;
  stRoot: string;
}

export interface AgentsResult<TEnriched extends boolean = false> {
  items: TEnriched extends true ? AgentSummaryEnriched[] : AgentSummary[];
}

// ─── Core ───────────────────────────────────────────────────────────────

export interface GetAgentsOpts {
  /** Filter to agents whose effective status matches. */
  status?: string | undefined;
  /** When true, return AgentSummaryEnriched[]; otherwise AgentSummary[]. */
  enrich?: boolean | undefined;
}

/**
 * Pure library-shaped enumeration. Same computation as {@link cmdAgents}
 * but takes positional `root` and returns the bare array (no `{items}`
 * envelope) — what `st.agents(...)` exposes to embedders.
 *
 * Read-only: walks `<root>/*` and consults each agent's status / name /
 * inbox. No writes.
 */
export function getAgents(
  root: string,
  opts: GetAgentsOpts = {}
): AgentSummary[] | AgentSummaryEnriched[] {
  const ids = listAgents(root);
  const base: AgentSummary[] = ids.map((id) => ({
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
  const enriched: AgentSummaryEnriched[] = filtered.map((m) => ({
    ...m,
    lastActivity: computeLastActivity(m.identity, root),
    inbox: computeInboxCount(m.identity, root),
  }));
  return enriched;
}

/**
 * CLI-shaped wrapper: keeps the `{items}` envelope return shape and the
 * input-object signature that existing callers (the MCP tools,
 * overview.ts, the existing test suite) depend on. Delegates to
 * {@link getAgents}; do not duplicate logic here.
 */
export function cmdAgents(
  input: AgentsInput
): AgentsResult<false> | AgentsResult<true> {
  const items = getAgents(input.stRoot, {
    ...(input.status !== undefined && { status: input.status }),
    ...(input.enrich !== undefined && { enrich: input.enrich }),
  });
  if (input.enrich === true) {
    return { items: items as AgentSummaryEnriched[] };
  }
  return { items: items as AgentSummary[] };
}

// ─── Helpers (exported for overview.ts) ─────────────────────────────────

/**
 * Walk `<root>/*` and return agent-shaped subfolders.
 *
 * Filters:
 *   - skip dotfiles (defensive; nothing in st uses them today)
 *   - skip non-directories
 *   - skip reserved names (defensive)
 *   - keep only names where validAgent(name) holds AND at least one of
 *     `<name>/inbox`, `<name>/archive` exists
 *
 * Returns alphabetically sorted.
 */
export function listAgents(root: string): string[] {
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
    if (!validAgent(name)) continue;
    const dir = join(root, name);
    let st: ReturnType<typeof statSync>;
    try {
      st = statSync(dir);
    } catch {
      continue;
    }
    if (!st.isDirectory()) continue;
    // An agent is a folder with an inbox/, an archive/, OR a status file.
    // The status-file case matters for a message-less CROSS-MACHINE agent:
    // the bus sync's `rsync --prune-empty-dirs` prunes its empty inbox/archive
    // so only its synced `status` file lands here — requiring inbox/archive
    // would then hide a live remote agent until it happens to have a message.
    if (
      !isDir(inboxDir(name, root)) &&
      !isDir(archiveDir(name, root)) &&
      !existsSync(statusPath(name, root))
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

/** Newest mtime across inbox/archive/status under <agent>/. */
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

function agentsHelp(name: string): string {
  return (
    `usage: ${name} agents [--status STATE] [--json [--enrich]]\n\n` +
    '  Enumerate every agent under $ST_ROOT — i.e. any sub-folder\n' +
    '  with at least one of inbox/ or archive/. Sorted alphabetically.\n' +
    '  Plain read; does not mutate state.\n\n' +
    '  --status STATE   only agents in STATE (available|busy|away|dnd|offline).\n' +
    '  --json           machine-readable array.\n' +
    '  --enrich         (with --json) add inbox counts + last-activity.\n\n' +
    '  Examples:\n' +
    `    ${name} agents                      # id / status / name, tab-separated\n` +
    `    ${name} agents --status available   # only agents marked available\n` +
    `    ${name} agents --json --enrich      # rich JSON (inbox + activity)\n\n` +
    `  Note: \`${name} members\` is the deprecated alias of this verb.\n`
  );
}

export function cmdAgentsCli(
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
        ctx.stderr(agentsHelp(invokedName(ctx.env)));
        return 0;
      default:
        throw new Error(`unknown flag: ${a}`);
    }
  }
  if (enrich && !json) {
    throw new Error('--enrich requires --json');
  }
  const r = cmdAgents({
    ...(status !== undefined && { status }),
    enrich,
    stRoot: ctx.stRoot,
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

// ─── Deprecated aliases (brief-009 item 3) ─────────────────────────────
//
// `members` was renamed to `agents`. The old names remain as
// @deprecated value-aliases pointing at the new exports for one
// release cycle so existing imports keep compiling.

/** @deprecated Use {@link AgentSummary}. */
export type MemberSummary = AgentSummary;
/** @deprecated Use {@link AgentSummaryEnriched}. */
export type MemberSummaryEnriched = AgentSummaryEnriched;
/** @deprecated Use {@link AgentsInput}. */
export type MembersInput = AgentsInput;
/** @deprecated Use {@link AgentsResult}. */
export type MembersResult<TEnriched extends boolean = false> = AgentsResult<TEnriched>;
/** @deprecated Use {@link GetAgentsOpts}. */
export type GetMembersOpts = GetAgentsOpts;
/** @deprecated Use {@link getAgents}. */
export const getMembers = getAgents;
/** @deprecated Use {@link cmdAgents}. */
export const cmdMembers = cmdAgents;
/** @deprecated Use {@link cmdAgentsCli}. */
export const cmdMembersCli = cmdAgentsCli;
/** @deprecated Use {@link listAgents}. */
export const listIdentities = listAgents;
