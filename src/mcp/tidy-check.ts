// mcp/tidy-check.ts — drift detector for the MCP tidy-check tick.
//
// brief-030: myobie audited two coord agents and caught a real pattern
// of execution drifting from the boot-ritual contract — one had 16
// unarchived inbox messages. The instructions are clear; agents drift
// anyway. So the MCP server gets a passive correction — a tidy-check
// tick that runs every TIDY_CHECK_INTERVAL_MS and, if drift holds,
// emits a synthetic `notifications/claude/channel` frame asking the
// agent to catch up.
//
// This file is the pure data-producing half. The tick scheduling +
// emit + dedup logic lives in mcp/index.ts.
//
// Drift conditions:
//   - inbox — any inbox file mtime > STALE_INBOX_MS old AND inbox count > 0

import { existsSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';

import {
  inboxDir,
  STALE_INBOX_MS,
  validFilename,
} from '../common.ts';

export interface DriftDetail {
  /** Count of inbox files older than STALE_INBOX_MS. 0 when the inbox
   *  condition didn't fire. */
  inboxStaleCount: number;
  /** Age (ms) of the oldest stale inbox file, or 0. */
  oldestInboxAgeMs: number;
}

export interface DriftResult {
  inbox: boolean;
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
  };

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

  const body = formatBody(inbox, detail);

  return { inbox, body, detail };
}

/** Convert a duration in ms to a short human reading: `47m`, `2h`, `3d`. */
function formatAge(ms: number): string {
  if (ms < 60_000) return `${Math.round(ms / 1000)}s`;
  if (ms < 60 * 60_000) return `${Math.round(ms / 60_000)}m`;
  if (ms < 24 * 60 * 60_000) return `${Math.round(ms / (60 * 60_000))}h`;
  return `${Math.round(ms / (24 * 60 * 60_000))}d`;
}

function formatBody(inbox: boolean, detail: DriftDetail): string {
  if (!inbox) return '';
  const n = detail.inboxStaleCount;
  const noun = n === 1 ? 'message' : 'messages';
  return (
    'Tidy check (drift detected):\n' +
    `- inbox: ${n} unaddressed ${noun} (oldest ${formatAge(detail.oldestInboxAgeMs)} old)`
  );
}
