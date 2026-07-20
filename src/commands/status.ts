// commands/status.ts — get or set <identity>/status.
//
// Per LAYOUT-004:
//   - status file content is exactly one of offline | available | busy | dnd.
//   - When the file is absent, the effective state is `offline`.
// Per brief-006 task 10:
//   - On read, never trust the file blindly: if the content is missing or
//     not in the vocabulary, normalize to `offline`.

import {
  existsSync,
  readFileSync,
  renameSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs';
import { randomBytes } from 'node:crypto';
import { dirname, join } from 'node:path';

import {
  resolveIdentity,
  SETTABLE_STATES,
  STATES,
  STATUS_LIVENESS_MS,
  STATUS_STALE_MS,
  type State,
  statusPath,
} from '../common.ts';
import { InvalidStateError } from '../errors.ts';

export interface StatusInput {
  recipient?: string | undefined;
  /** Present iff the user passed `--set <state>`. */
  setState?: string | undefined;
  env: NodeJS.ProcessEnv;
  stRoot: string;
}

export type StatusResult =
  | { mode: 'get'; identity: string; state: State }
  | { mode: 'set'; identity: string; state: State; written: string };

export function cmdStatus(input: StatusInput): StatusResult {
  // `--set` is a newcomer's first command per the docs, and the
  // onboarding walkthrough promises lazy-bootstrap of the agent
  // folder. The `'lazy-create'` policy makes an explicit-identity
  // `--set` create `<agent>/{inbox,archive}` if missing instead of
  // failing with "agent folder missing". Passive `get` and env-
  // resolved identities keep their original behavior.
  const isSet = input.setState !== undefined;
  const identity = resolveIdentity({
    explicit: input.recipient,
    env: input.env,
    stRoot: input.stRoot,
    ...(isSet ? { policy: 'lazy-create' as const } : {}),
  });
  const path = statusPath(identity, input.stRoot);

  if (input.setState === undefined) {
    return { mode: 'get', identity, state: readState(path) };
  }

  // `unknown` is derived from mtime staleness — it's never settable by
  // the user. Reject it explicitly before the broader isValidState check
  // so the error message stays accurate.
  if (!isSettableState(input.setState)) {
    throw new InvalidStateError(input.setState);
  }
  const state = input.setState as State;
  writeFileSync(path, `${state}\n`);
  return { mode: 'set', identity, state, written: state };
}

function readState(path: string): State {
  if (!existsSync(path)) return 'offline';
  // mtime staleness: if the file hasn't been touched in
  // STATUS_STALE_MS, whatever it says is no longer trustworthy. Treat
  // as `unknown`. Distinct from missing-file (offline) and from
  // corrupted-contents (also offline, brief-006).
  try {
    const st = statSync(path);
    if (Date.now() - st.mtimeMs > STATUS_STALE_MS) {
      return 'unknown';
    }
  } catch {
    return 'offline';
  }
  let raw: string;
  try {
    raw = readFileSync(path, 'utf8');
  } catch {
    return 'offline';
  }
  // First line, whitespace-trimmed.
  const firstLine = raw.split('\n')[0] ?? '';
  const trimmed = firstLine.replace(/[\s]/g, '');
  if (isValidState(trimmed)) return trimmed as State;
  // Brief-006 rule: never trust the file blindly. Garbage normalizes to
  // offline so a corrupt status file doesn't propagate.
  return 'offline';
}

/**
 * Read `<id>/status` without going through resolveIdentity (which
 * auto-creates folders). Used by `st members` and `st overview`
 * for passive cross-tree enumeration. Same permissive rules as the
 * `st status` getter: missing file → `offline`, malformed
 * contents → `offline`.
 */
export function readIdentityStatus(identity: string, root: string): State {
  return readState(statusPath(identity, root));
}

/**
 * The shared freshness verdict for one identity's status file.
 *
 * `st agents` said `available` at the same instant `convoy ls --tree`
 * said `DEAD (status stale 3m ago)`. Both were reading the same file;
 * they disagreed because each had picked its own window. This is the
 * one definition every consumer should read through so that stops
 * happening.
 *
 * Note the two fields are answers to two different questions, and a
 * caller generally wants both:
 *
 *   `status`   — what `st status` / `readIdentityStatus` return. Applies
 *                the TRUST window (STATUS_STALE_MS, 15 min) and so
 *                collapses to `unknown` only when the value itself is
 *                no longer believable.
 *   `live`     — whether the agent is demonstrably alive right now,
 *                per the LIVENESS window (STATUS_LIVENESS_MS, 2 min).
 *
 * `recorded` deliberately survives both windows. An identity that was
 * `busy` and went stale is NOT the same as one that cleanly went
 * `offline`, and collapsing them loses the signal that tells you
 * "something died mid-work" from "something shut down".
 */
export interface StatusLiveness {
  /** Derived state, trust-windowed. Same value `st status` prints. */
  status: State;
  /** The literal on-disk value, whatever its age. `null` when the file
   *  is missing or unreadable — distinct from a recorded `offline`. */
  recorded: State | null;
  /** Age of the status file's mtime in ms, relative to the `now` this
   *  verdict was taken at; `null` when there is no file. Point-in-time
   *  by nature — persist {@link mtimeMs} instead if you need something
   *  that compares equal across two reads of an unchanged bus. */
  ageMs: number | null;
  /** Absolute mtime of the status file; `null` when there is no file. */
  mtimeMs: number | null;
  /** True iff the file exists and is fresher than STATUS_LIVENESS_MS. */
  live: boolean;
}

/**
 * Read an identity's status once and return the full freshness verdict.
 * Single stat + single read; the shared contract behind `st agents`.
 */
export function readIdentityLiveness(
  identity: string,
  root: string,
  opts: { livenessMs?: number; now?: number } = {}
): StatusLiveness {
  const path = statusPath(identity, root);
  const livenessMs = opts.livenessMs ?? STATUS_LIVENESS_MS;
  const now = opts.now ?? Date.now();
  const status = readState(path);

  let mtimeMs: number | null = null;
  try {
    if (existsSync(path)) mtimeMs = statSync(path).mtimeMs;
  } catch {
    mtimeMs = null;
  }
  const ageMs = mtimeMs === null ? null : now - mtimeMs;

  let recorded: State | null = null;
  if (mtimeMs !== null) {
    try {
      const first = (readFileSync(path, 'utf8').split('\n')[0] ?? '')
        .replace(/[\s]/g, '');
      if (isValidState(first)) recorded = first as State;
    } catch {
      recorded = null;
    }
  }

  return {
    status,
    recorded,
    ageMs,
    mtimeMs,
    live: ageMs !== null && ageMs <= livenessMs,
  };
}

export function isValidState(s: string): s is State {
  return (STATES as readonly string[]).includes(s);
}

/** Like {@link isValidState} but excludes the derived `unknown` state.
 *  Used by the `--set` validator. */
export function isSettableState(s: string): s is State {
  return (SETTABLE_STATES as readonly string[]).includes(s);
}

/** Outcome of a {@link refreshIdentityStatus} call. */
export type RefreshOutcome =
  | 'refreshed' // file present + valid state → re-wrote same value, mtime bumped
  | 'wrote-default' // file missing → wrote `available` as the sensible default
  | 'left-corrupt' // file present but content isn't a settable state → untouched
  | 'left-unknown' // file recorded `unknown` (which we never write) → untouched
  | 'error'; // read or write threw — best-effort, caller decides whether to log

/**
 * brief-023 + brief-032: re-write an identity's status file to bump
 * mtime so peers don't see them fall into `unknown` (mtime > 15 min
 * stale) while they're still alive. Both the MCP server's
 * 5-min-interval tick AND `st ding`'s mirror tick call this; same
 * logic per identity, no matter which surface is running.
 *
 * Rules:
 *  - File missing → write `available` (sensible default for a
 *    connected agent; the boot ritual should have set this, this is
 *    the fallback).
 *  - File contains a valid settable state (offline/available/busy/
 *    away/dnd) → re-write the same value. Preserves user intent
 *    (busy stays busy) while bumping mtime.
 *  - File contains `unknown` somehow → untouched. We never write
 *    `unknown` deliberately; if it appears we don't re-assert it.
 *  - File corrupt → untouched. We'd rather peers see the staleness
 *    fallback than have us invent a value.
 *
 * Writes are atomic via tmp + rename so a concurrent reader can't
 * see a partial file.
 */
export function refreshIdentityStatus(
  identity: string,
  root: string
): RefreshOutcome {
  const path = statusPath(identity, root);
  let exists: boolean;
  try {
    exists = existsSync(path);
  } catch {
    return 'error';
  }
  if (!exists) {
    return writeStatusAtomic(path, 'available')
      ? 'wrote-default'
      : 'error';
  }
  let raw: string;
  try {
    raw = readFileSync(path, 'utf8');
  } catch {
    return 'error';
  }
  const first = (raw.split('\n')[0] ?? '').trim();
  if (first === 'unknown') return 'left-unknown';
  if (!isSettableState(first)) return 'left-corrupt';
  return writeStatusAtomic(path, first) ? 'refreshed' : 'error';
}

/** Atomic status write: tmp sibling + rename. Concurrent readers see
 *  either the old bytes or the new bytes, never a partial file. */
function writeStatusAtomic(path: string, value: string): boolean {
  const dir = dirname(path);
  const tmp = join(
    dir,
    `.status.tmp-${process.pid}-${randomBytes(3).toString('hex')}`
  );
  try {
    writeFileSync(tmp, `${value}\n`);
    renameSync(tmp, path);
    return true;
  } catch {
    // best-effort cleanup of the tmp file if rename failed
    try {
      unlinkSync(tmp);
    } catch {
      // ignore
    }
    return false;
  }
}

export { cmdStatus as cmdStatusCore };

// ─── CLI wrapper ────────────────────────────────────────────────────────

import { invokedName, type CliContext } from '../cli-context.ts';

export function cmdStatusCli(
  args: readonly string[],
  ctx: CliContext
): number {
  let recipient: string | undefined;
  let setState: string | undefined;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '--set':
        if (i + 1 >= args.length) {
          throw new Error('--set requires a state value');
        }
        setState = args[++i];
        break;
      case '-h':
      case '--help':
        {
          const name = invokedName(ctx.env);
          ctx.stderr(
            `usage: ${name} status [<identity>] [--set <state>]\n\n` +
              '  Get or set an agent status. No args prints your own\n' +
              '  ($ST_AGENT) status; <identity> reads another agent\'s.\n' +
              '  --set <state> writes your own — states: available | busy |\n' +
              '  away | dnd | offline.\n\n' +
              '  Examples:\n' +
              `    ${name} status --set busy      # go heads-down (ding suppresses)\n` +
              `    ${name} status                 # print my current status\n` +
              `    ${name} status alice           # read alice's status\n`
          );
        }
        return 0;
      default:
        if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
        if (recipient === undefined) recipient = a;
        else throw new Error(`unexpected arg: ${a}`);
    }
  }
  const r = cmdStatus({
    ...(recipient !== undefined && { recipient }),
    ...(setState !== undefined && { setState }),
    env: ctx.env,
    stRoot: ctx.stRoot,
  });
  if (r.mode === 'get') {
    ctx.stdout(`${r.state}\n`);
  } else {
    ctx.stdout(`status: ${r.state}\n`);
  }
  return 0;
}
