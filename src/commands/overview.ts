// commands/overview.ts — synthesized at-a-glance dashboard.
//
// Composes existing read paths (agents, ls, raw mtimes) into one
// snapshot for the active agent. Designed for "what's the state of
// my st world right now?" — typed for embedders via --json; text
// for humans by default.

import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';

import { invokedName, type CliContext } from '../cli-context.ts';
import {
  archiveDir,
  filenameTimestamp,
  inboxDir,
  msNow,
  parseFrontmatter,
  resolveIdentity,
  statusPath,
  validFilename,
} from '../common.ts';
import type { Filename, Identity } from '../types.ts';

import { cmdAgents, type AgentSummaryEnriched } from './agents.ts';

export interface OverviewInboxOldest {
  filename: Filename;
  from: Identity;
  subject?: string;
  ageMs: number;
}

export interface OverviewInbox {
  unread: number;
  oldest: OverviewInboxOldest | null;
}

export type ActivityKind = 'message' | 'archive' | 'status';

export interface OverviewActivity {
  kind: ActivityKind;
  identity: Identity;
  target?: Identity;
  subject?: string;
  ageMs: number;
  filename?: Filename;
}

export interface Overview {
  identity: Identity;
  inbox: OverviewInbox;
  /** Field name kept as `members` for back-compat with embedder
   *  destructures; will rename to `agents` in a follow-up. */
  members: AgentSummaryEnriched[];
  recent: OverviewActivity[];
}

export interface OverviewInput {
  recent?: number | undefined;
  /** Override now() for deterministic tests. */
  now?: () => number;
  env: NodeJS.ProcessEnv;
  stRoot: string;
}

const DEFAULT_RECENT_N = 10;

// ─── Core ───────────────────────────────────────────────────────────────

export interface GetOverviewOpts {
  /** How many entries to keep on the recent-activity list (default 10). */
  recent?: number | undefined;
  /** Override now() for deterministic tests. */
  now?: () => number;
}

/**
 * Pure library-shaped overview computation. Takes positional `root` and
 * `identity` (no env resolution) so an embedder can render a dashboard
 * for any identity without going through `$COORD_IDENTITY`. Read-only.
 * Added in brief-028 to back `st.overview(...)` on the smalltalk instance handle.
 */
export function getOverview(
  root: string,
  identity: Identity,
  opts: GetOverviewOpts = {}
): Overview {
  const now = opts.now ?? msNow;

  const members = (
    cmdAgents({ enrich: true, stRoot: root }).items as
      AgentSummaryEnriched[]
  );

  const inbox = computeInboxSummary(identity, root, now);
  const recent = computeRecentActivity(
    members.map((m) => m.identity),
    root,
    now(),
    opts.recent ?? DEFAULT_RECENT_N
  );

  return { identity, inbox, members, recent };
}

/**
 * CLI-shaped wrapper retained for the dispatcher / existing tests:
 * resolves identity from env, then delegates to {@link getOverview}.
 */
export function cmdOverview(input: OverviewInput): Overview {
  const identity = resolveIdentity({
    env: input.env,
    stRoot: input.stRoot,
  }) as Identity;
  return getOverview(input.stRoot, identity, {
    ...(input.recent !== undefined && { recent: input.recent }),
    ...(input.now !== undefined && { now: input.now }),
  });
}

// ─── Inbox summary ──────────────────────────────────────────────────────

function computeInboxSummary(
  identity: string,
  root: string,
  now: () => number
): OverviewInbox {
  const dir = inboxDir(identity, root);
  if (!existsSync(dir)) return { unread: 0, oldest: null };
  let names: string[];
  try {
    names = readdirSync(dir);
  } catch {
    return { unread: 0, oldest: null };
  }
  const valid = names.filter((n) => validFilename(n)).sort();
  if (valid.length === 0) return { unread: 0, oldest: null };
  const firstName = valid[0]!;
  const oldest = parseInboxItem(
    identity,
    firstName,
    join(dir, firstName),
    now()
  );
  return { unread: valid.length, oldest };
}

function parseInboxItem(
  _recipient: string,
  filename: string,
  path: string,
  nowMs: number
): OverviewInboxOldest | null {
  let text: string;
  try {
    text = readFileSync(path, 'utf8');
  } catch {
    return null;
  }
  const { fm } = parseFrontmatter(text);
  const from =
    typeof fm.from === 'string' && fm.from.length > 0
      ? (fm.from as Identity)
      : ('unknown' as Identity);
  const subject =
    typeof fm.subject === 'string' && fm.subject.length > 0
      ? fm.subject
      : undefined;
  let ageMs: number;
  try {
    ageMs = Math.max(0, nowMs - filenameTimestamp(filename));
  } catch {
    ageMs = 0;
  }
  const result: OverviewInboxOldest = {
    filename: filename as Filename,
    from,
    ageMs,
  };
  if (subject !== undefined) result.subject = subject;
  return result;
}

// ─── Recent activity ────────────────────────────────────────────────────

function computeRecentActivity(
  identities: string[],
  root: string,
  nowMs: number,
  topN: number
): OverviewActivity[] {
  interface Entry {
    mtimeMs: number;
    activity: OverviewActivity;
  }
  const entries: Entry[] = [];

  const consider = (
    path: string,
    mkActivity: () => OverviewActivity | null
  ): void => {
    let st: ReturnType<typeof statSync>;
    try {
      st = statSync(path);
    } catch {
      return;
    }
    if (!st.isFile()) return;
    const activity = mkActivity();
    if (activity === null) return;
    activity.ageMs = Math.max(0, nowMs - st.mtimeMs);
    entries.push({ mtimeMs: st.mtimeMs, activity });
  };

  for (const id of identities) {
    // Messages currently in <id>/inbox.
    for (const dir of [inboxDir(id, root), archiveDir(id, root)] as const) {
      if (!existsSync(dir)) continue;
      const kind: ActivityKind = dir.endsWith('inbox') ? 'message' : 'archive';
      let names: string[];
      try {
        names = readdirSync(dir);
      } catch {
        continue;
      }
      for (const name of names) {
        if (!validFilename(name)) continue;
        const path = join(dir, name);
        consider(path, () => {
          const { fm } = readFmSafe(path);
          const sender = typeof fm.from === 'string' ? fm.from : 'unknown';
          const subject =
            typeof fm.subject === 'string' && fm.subject.length > 0
              ? fm.subject
              : undefined;
          const a: OverviewActivity = {
            kind,
            identity: sender as Identity,
            target: id as Identity,
            ageMs: 0, // overwritten by consider()
            filename: name as Filename,
          };
          if (subject !== undefined) a.subject = subject;
          return a;
        });
      }
    }
    // Status file (single, identity-scoped).
    const sp = statusPath(id, root);
    if (existsSync(sp)) {
      consider(sp, () => ({
        kind: 'status',
        identity: id as Identity,
        ageMs: 0,
      }));
    }
  }

  entries.sort((a, b) => b.mtimeMs - a.mtimeMs);
  return entries.slice(0, topN).map((e) => e.activity);
}

function readFmSafe(path: string): { fm: Record<string, unknown>; body: string } {
  try {
    const text = readFileSync(path, 'utf8');
    return parseFrontmatter(text);
  } catch {
    return { fm: {}, body: '' };
  }
}

// ─── Text formatting ────────────────────────────────────────────────────

function formatText(o: Overview): string {
  const lines: string[] = [];
  lines.push(`You are ${o.identity}.`);
  lines.push('');
  lines.push('Inbox:');
  if (o.inbox.unread === 0) {
    lines.push('  (empty)');
  } else if (o.inbox.oldest !== null) {
    const subj = o.inbox.oldest.subject ?? '(no subject)';
    lines.push(
      `  ${o.inbox.unread} new (oldest ${formatAge(o.inbox.oldest.ageMs)} ago: ${o.inbox.oldest.from} — "${subj}")`
    );
  } else {
    lines.push(`  ${o.inbox.unread} new`);
  }
  lines.push('');
  lines.push(`Members (${o.members.length}):`);
  if (o.members.length === 0) {
    lines.push('  (none)');
  } else {
    for (const m of o.members) {
      let line = `  ${pad(m.identity, 12)} ${pad(m.status, 10)} `;
      if (m.lastActivity !== null) {
        line += `last active ${formatAge(Math.max(0, msNow() - m.lastActivity))} ago`;
      } else {
        line += '—';
      }
      lines.push(line);
    }
  }
  lines.push('');
  lines.push('Recent activity:');
  if (o.recent.length === 0) {
    lines.push('  (none)');
  } else {
    for (const r of o.recent) {
      lines.push(
        `  ${pad(formatAge(r.ageMs), 5)} ${formatActivityLine(r)}`
      );
    }
  }
  return lines.join('\n') + '\n';
}

function formatActivityLine(r: OverviewActivity): string {
  switch (r.kind) {
    case 'message':
      return `${r.identity} → ${r.target ?? '?'} — ${r.subject ?? r.filename ?? ''}`;
    case 'archive':
      return `archive ${r.identity} → ${r.target ?? '?'} — ${r.subject ?? r.filename ?? ''}`;
    case 'status':
      return `${r.identity} status changed`;
  }
}

function formatAge(ms: number): string {
  if (ms < 0) ms = 0;
  if (ms < 60_000) return `${Math.floor(ms / 1000)}s`;
  if (ms < 3_600_000) return `${Math.floor(ms / 60_000)}m`;
  if (ms < 86_400_000) return `${Math.floor(ms / 3_600_000)}h`;
  return `${Math.floor(ms / 86_400_000)}d`;
}

function pad(s: string, width: number): string {
  return s.length >= width ? s : s + ' '.repeat(width - s.length);
}

// ─── CLI wrapper ────────────────────────────────────────────────────────

function overviewHelp(name: string): string {
  return (
    `usage: ${name} overview [--recent N] [--json]\n\n` +
    '  At-a-glance snapshot for $COORD_IDENTITY:\n' +
    '    - your inbox unread count + oldest item\n' +
    '    - every identity\'s status + last-activity\n' +
    '    - top N recent activity entries across the tree\n'
  );
}

export function cmdOverviewCli(
  args: readonly string[],
  ctx: CliContext
): number {
  let recent: number | undefined;
  let json = false;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '--recent': {
        const v = args[++i];
        if (v === undefined || !/^[0-9]+$/.test(v)) {
          throw new Error('--recent must be a non-negative integer');
        }
        recent = Number(v);
        break;
      }
      case '--json':
        json = true;
        break;
      case '-h':
      case '--help':
        ctx.stderr(overviewHelp(invokedName(ctx.env)));
        return 0;
      default:
        throw new Error(`unknown flag: ${a}`);
    }
  }
  const r = cmdOverview({
    ...(recent !== undefined && { recent }),
    env: ctx.env,
    stRoot: ctx.stRoot,
  });
  if (json) {
    ctx.stdout(`${JSON.stringify(r)}\n`);
    return 0;
  }
  ctx.stdout(formatText(r));
  return 0;
}
