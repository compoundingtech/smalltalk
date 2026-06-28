// mcp/tidy-check.ts — drift detector for the MCP tidy-check tick.
//
// brief-030: myobie audited two coord agents and caught a real pattern
// of execution drifting from the boot-ritual contract: one had 16
// unarchived inbox messages and was journaling ~10 briefs behind. The
// instructions are clear; agents drift anyway. So the MCP server gets
// a passive correction — a tidy-check tick that runs every
// TIDY_CHECK_INTERVAL_MS and, if any drift condition holds, emits a
// synthetic `notifications/claude/channel` frame asking the agent to
// catch up.
//
// This file is the pure data-producing half. The tick scheduling +
// emit + dedup logic lives in mcp/index.ts.
//
// Drift conditions:
//   - inbox    — any inbox file mtime > STALE_INBOX_MS old AND inbox count > 0
//   - journal  — latest journal entry mtime > STALE_JOURNAL_MS old

import { existsSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';

import {
  inboxDir,
  journalDir,
  STALE_INBOX_MS,
  STALE_JOURNAL_MS,
  validFilename,
} from '../common.ts';

// Journal filename grammar mirrors commands/journal.ts — permissive on
// the slug. Duplicated locally to avoid pulling journal.ts (which
// pulls editor/spawnSync deps) into the MCP module hot path.
const JOURNAL_FILENAME_RE = /^[0-9]{13}-[A-Za-z0-9._-]+\.md$/;

export interface DriftDetail {
  /** Count of inbox files older than STALE_INBOX_MS. 0 when the inbox
   *  condition didn't fire. */
  inboxStaleCount: number;
  /** Age (ms) of the oldest stale inbox file, or 0. */
  oldestInboxAgeMs: number;
  /** Age (ms) since the latest journal entry, or 0 when the journal
   *  condition didn't fire. */
  journalLagMs: number;
}

export interface DriftResult {
  inbox: boolean;
  journal: boolean;
  /** Human-readable summary suitable for use as the synthetic channel
   *  frame's `content`. Empty when no condition fired. */
  body: string;
  /** Structured detail behind the booleans, exposed for tests and for
   *  the emit-side dedup logic that may want to log specifics. */
  detail: DriftDetail;
}

export interface EvaluateDriftOpts {
  /** Override Date.now() for deterministic tests. */
  now?: () => number;
}

/**
 * Evaluate the drift conditions against the identity's folders under
 * `root`. Read-only — no I/O beyond statSync / readdirSync.
 *
 * Returns the booleans plus a pre-formatted body and structured
 * detail. Caller (the tick) decides whether to emit based on dedup
 * state.
 */
export function evaluateDrift(
  identity: string,
  root: string,
  opts: EvaluateDriftOpts = {}
): DriftResult {
  const now = opts.now ?? Date.now;
  const nowMs = now();

  const detail: DriftDetail = {
    inboxStaleCount: 0,
    oldestInboxAgeMs: 0,
    journalLagMs: 0,
  };

  // ─ Inbox condition ─
  const ibox = inboxDir(identity, root);
  if (existsSync(ibox)) {
    let entries: string[];
    try {
      entries = readdirSync(ibox);
    } catch {
      entries = [];
    }
    for (const name of entries) {
      if (!validFilename(name)) continue;
      let st: ReturnType<typeof statSync>;
      try {
        st = statSync(join(ibox, name));
      } catch {
        continue;
      }
      const age = nowMs - st.mtimeMs;
      if (age > STALE_INBOX_MS) {
        detail.inboxStaleCount++;
        if (age > detail.oldestInboxAgeMs) detail.oldestInboxAgeMs = age;
      }
    }
  }
  const inbox = detail.inboxStaleCount > 0;

  // ─ Journal-lag condition ─
  // Latest journal entry mtime older than STALE_JOURNAL_MS triggers.
  // Missing journal folder behaves as "no entries" (mtime 0) — won't
  // trigger because there's nothing to be stale about.
  let latestJournalMtime = 0;
  const jdir = journalDir(identity, root);
  if (existsSync(jdir)) {
    let names: string[];
    try {
      names = readdirSync(jdir);
    } catch {
      names = [];
    }
    for (const name of names) {
      if (!JOURNAL_FILENAME_RE.test(name)) continue;
      let st: ReturnType<typeof statSync>;
      try {
        st = statSync(join(jdir, name));
      } catch {
        continue;
      }
      if (st.mtimeMs > latestJournalMtime) latestJournalMtime = st.mtimeMs;
    }
  }
  const journal =
    latestJournalMtime > 0 &&
    nowMs - latestJournalMtime > STALE_JOURNAL_MS;
  if (journal) {
    detail.journalLagMs = nowMs - latestJournalMtime;
  }

  // ─ Body ─
  const body = formatBody(inbox, journal, detail);

  return { inbox, journal, body, detail };
}

/** Convert a duration in ms to a short human reading: `47m`, `2h`, `3d`. */
function formatAge(ms: number): string {
  if (ms < 60_000) return `${Math.round(ms / 1000)}s`;
  if (ms < 60 * 60_000) return `${Math.round(ms / 60_000)}m`;
  if (ms < 24 * 60 * 60_000) return `${Math.round(ms / (60 * 60_000))}h`;
  return `${Math.round(ms / (24 * 60 * 60_000))}d`;
}

function formatBody(
  inbox: boolean,
  journal: boolean,
  detail: DriftDetail
): string {
  if (!inbox && !journal) return '';
  const lines: string[] = ['Tidy check (drift detected):'];
  if (inbox) {
    const n = detail.inboxStaleCount;
    const noun = n === 1 ? 'message' : 'messages';
    lines.push(
      `- inbox: ${n} unaddressed ${noun} (oldest ${formatAge(detail.oldestInboxAgeMs)} old)`
    );
  }
  if (journal) {
    lines.push(
      `- No journal entry for ${formatAge(detail.journalLagMs)}. Consider dropping a terse journal entry.`
    );
  }
  return lines.join('\n');
}
