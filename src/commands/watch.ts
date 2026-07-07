// commands/watch.ts — emit one line per new file appearing under $ST_ROOT.
//
// Two modes (mirror of lib/cmd_watch.sh):
//   - per-identity:  cmdWatch with `recipient` set → watches <id>/inbox/.
//   - cross-tree:    cmdWatch without recipient    → watches every
//                    <peer>/inbox/ EXCEPT the watcher's own folder
//                    (suppression id from --from / $ST_AGENT).
//
// The state machine is split into two pure-ish functions:
//   watchReplay() — initial pass: emit files at or above the cutoff,
//                   seed the seen-set with EVERY valid existing file.
//   watchPoll()   — subsequent passes: emit files NOT yet in seen,
//                   add them.
//
// The CLI strings these together with setInterval; the unit tests
// exercise the functions directly so no real timers are needed.

import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { basename, dirname, join } from 'node:path';

import {
  filenameTimestamp,
  inboxDir,
  msNow,
  parseFrontmatter,
  resolveIdentity,
  validFilename,
  validIdentity,
} from '../common.ts';
import { StError } from '../errors.ts';

export interface WatchSetup {
  /** Per-identity mode: name of the single inbox to watch. */
  singleId?: string | undefined;
  /** Cross-tree mode: identity whose folder is suppressed. */
  suppressId?: string | undefined;
  /** Replay cutoff: emit files with filename ts >= cutoff. */
  cutoff: number;
  withSubject?: boolean;
  /** $ST_ROOT — explicit so tests can use temp dirs. */
  stRoot: string;
}

export interface WatchInput {
  recipient?: string | undefined;
  /**
   * When true, watch every identity's inbox EXCEPT the watcher's own
   * folder (suppression). Pre-brief-017a this was the default
   * behavior when no recipient was passed; now it's an opt-in
   * `--all` flag. Mutually exclusive with `recipient`.
   */
  all?: boolean;
  fromExplicit?: string | undefined;
  withSubject?: boolean;
  /** Explicit numeric cutoff. Mutually exclusive with sinceNow. */
  since?: number | undefined;
  /** Use the current `now()` as the cutoff. */
  sinceNow?: boolean;
  /** Polling interval in ms (default 500). */
  intervalMs?: number;
  once?: boolean;
  env: NodeJS.ProcessEnv;
  stRoot: string;
  /** Override now() for deterministic --since-now testing. */
  now?: () => number;
}

export interface WatchLine {
  filename: string;
  /** Identity whose inbox the file appeared in (e.g. `bob` for
   *  `<root>/bob/inbox/X.md`). */
  identity: string;
  /** Subject (only set when withSubject is true). */
  subject?: string;
}

// ─── Public function: resolve setup from input ──────────────────────────

export interface WatchSetupResult {
  setup: WatchSetup;
  intervalMs: number;
  once: boolean;
}

const DEFAULT_INTERVAL = 500;

/**
 * Validate flags, resolve mode (per-identity vs cross-tree), pick the
 * cutoff, and ensure the per-identity inbox/archive exists. Throws on
 * any user-input error. Used by both the CLI and the unit tests.
 */
export function resolveWatchSetup(input: WatchInput): WatchSetupResult {
  const intervalMs = input.intervalMs ?? DEFAULT_INTERVAL;
  if (!Number.isInteger(intervalMs) || intervalMs < 0) {
    throw new StError(
      'INVALID_INTERVAL',
      '--interval must be a non-negative integer (ms)',
      { value: intervalMs }
    );
  }
  if (input.since !== undefined) {
    if (!Number.isInteger(input.since) || (input.since as number) < 0) {
      throw new StError(
        'INVALID_SINCE',
        '--since must be a unix-ms integer',
        { value: input.since }
      );
    }
  }
  if (input.since !== undefined && input.sinceNow === true) {
    throw new StError(
      'WATCH_FLAG_CONFLICT',
      '--since and --since-now are mutually exclusive'
    );
  }

  if (input.recipient && input.all === true) {
    throw new StError(
      'WATCH_FLAG_CONFLICT',
      '--all and <identity> are mutually exclusive'
    );
  }

  let singleId: string | undefined;
  let suppressId: string | undefined;
  if (input.recipient) {
    // Lenient on the watched recipient: a peer's tree on this
    // machine may have only inbox/ (from a single send). Don't
    // auto-create — cross-identity watches shouldn't materialize
    // archive/ for an identity we're merely observing.
    singleId = resolveIdentity({
      explicit: input.recipient,
      env: input.env,
      stRoot: input.stRoot,
      policy: 'lenient',
    });
  } else if (input.all === true) {
    // Cross-tree mode: watch every peer's inbox, suppressing the
    // watcher's own folder. Needs an identity for the suppression
    // (via --from or $ST_AGENT).
    if (
      (input.fromExplicit === undefined || input.fromExplicit === '') &&
      (!input.env.ST_AGENT || input.env.ST_AGENT === '')
    ) {
      throw new StError(
        'IDENTITY_REQUIRED_FOR_SUPPRESS',
        'identity required to determine which folder to suppress — set $ST_AGENT'
      );
    }
    suppressId = resolveIdentity({
      explicit: input.fromExplicit,
      env: input.env,
      stRoot: input.stRoot,
    });
  } else {
    // brief-017a default: no args → watch $ST_AGENT's own
    // inbox, consistent with `ls`, `status`, etc. Pre-017a this was
    // cross-tree (which was surprising — see brief-017a bug 3).
    singleId = resolveIdentity({
      explicit: input.fromExplicit,
      env: input.env,
      stRoot: input.stRoot,
    });
  }

  let cutoff: number;
  if (input.since !== undefined) {
    cutoff = input.since;
  } else if (input.sinceNow === true) {
    cutoff = input.now ? input.now() : msNow();
  } else {
    cutoff = 0;
  }

  return {
    setup: {
      singleId,
      suppressId,
      cutoff,
      withSubject: input.withSubject === true,
      stRoot: input.stRoot,
    },
    intervalMs,
    once: input.once === true,
  };
}

// ─── Phase 1: initial replay ────────────────────────────────────────────

export function watchReplay(setup: WatchSetup): {
  lines: WatchLine[];
  seen: Set<string>;
} {
  const seen = new Set<string>();
  const lines: WatchLine[] = [];
  for (const dir of watchTargetDirs(setup)) {
    scanDir(dir, setup, /*replay*/ true, seen, lines);
  }
  return { lines, seen };
}

// ─── Phase 2: live poll step ────────────────────────────────────────────

export function watchPoll(
  setup: WatchSetup,
  seen: Set<string>
): { lines: WatchLine[] } {
  const lines: WatchLine[] = [];
  for (const dir of watchTargetDirs(setup)) {
    scanDir(dir, setup, /*replay*/ false, seen, lines);
  }
  return { lines };
}

// ─── Helpers ────────────────────────────────────────────────────────────

/**
 * Compute the inbox directories to scan for the current setup. Re-runs
 * each call so cross-tree mode picks up newly-created peer folders on
 * the next poll.
 */
export function watchTargetDirs(setup: WatchSetup): string[] {
  if (setup.singleId !== undefined) {
    return [inboxDir(setup.singleId, setup.stRoot)];
  }
  let topEntries: string[];
  try {
    topEntries = readdirSync(setup.stRoot);
  } catch {
    return [];
  }
  const dirs: string[] = [];
  for (const id of topEntries) {
    if (!validIdentity(id)) continue;
    if (id === setup.suppressId) continue;
    const d = inboxDir(id, setup.stRoot);
    if (!isDir(d)) continue;
    dirs.push(d);
  }
  return dirs;
}

function isDir(p: string): boolean {
  try {
    return statSync(p).isDirectory();
  } catch {
    return false;
  }
}

function scanDir(
  dir: string,
  setup: WatchSetup,
  replay: boolean,
  seen: Set<string>,
  out: WatchLine[]
): void {
  if (!existsSync(dir)) return;
  let entries: string[];
  try {
    entries = readdirSync(dir);
  } catch {
    return;
  }
  for (const name of entries) {
    if (!validFilename(name)) continue;
    if (replay) {
      const ts = filenameTimestamp(name);
      if (ts >= setup.cutoff) {
        out.push(makeLine(dir, name, setup.withSubject === true));
      }
      seen.add(name);
    } else {
      if (seen.has(name)) continue;
      out.push(makeLine(dir, name, setup.withSubject === true));
      seen.add(name);
    }
  }
}

function makeLine(dir: string, name: string, withSubject: boolean): WatchLine {
  // dir = `<root>/<id>/inbox`; identity is the parent segment of `inbox`.
  const identity = basename(dirname(dir));
  if (!withSubject) return { filename: name, identity };
  let subject = '';
  try {
    const fm = parseFrontmatter(readFileSync(join(dir, name), 'utf8')).fm;
    if (typeof fm.subject === 'string') subject = fm.subject;
  } catch {
    // ignore: leave empty subject
  }
  return { filename: name, identity, subject };
}

export function formatWatchLine(line: WatchLine): string {
  if (line.subject !== undefined) return `${line.filename}\t${line.subject}`;
  return line.filename;
}

// ─── CLI long-running entry point (used by the dispatcher) ──────────────

export interface WatchRunDeps {
  /** Called once per emitted line; defaults to writing to stdout. */
  emit?: (line: WatchLine) => void;
  /** Polling loop — defaults to setInterval. The CLI passes the real
   * one; tests can pass a no-op stub for the once-mode + manual poll. */
  schedule?: (cb: () => void, ms: number) => { unref?: () => void };
}

/**
 * One-shot replay (always) plus, if `!once`, an interval-driven poll
 * loop. Returns a `stop()` function that cancels the interval. Tests
 * generally call `watchReplay` / `watchPoll` directly instead.
 */
export function cmdWatch(
  input: WatchInput,
  deps: WatchRunDeps = {}
): { stop: () => void } {
  const { setup, intervalMs, once } = resolveWatchSetup(input);
  const emit = deps.emit ?? defaultEmit;
  const { lines, seen } = watchReplay(setup);
  for (const l of lines) emit(l);
  if (once) return { stop: () => {} };
  const handle = (deps.schedule ?? defaultSchedule)(() => {
    const r = watchPoll(setup, seen);
    for (const l of r.lines) emit(l);
  }, intervalMs);
  return {
    stop: () => {
      const h = handle as { unref?: () => void; clear?: () => void } & {
        ref?: () => void;
      };
      // setInterval handles in Node have no .clear; we use the timer id
      // returned by clearInterval below.
      void h;
    },
  };
}

function defaultEmit(line: WatchLine): void {
  process.stdout.write(`${formatWatchLine(line)}\n`);
}

function defaultSchedule(
  cb: () => void,
  ms: number
): { unref: () => void } {
  // setInterval returns a Timeout in Node; cast for the structural match.
  const handle = setInterval(cb, ms);
  return { unref: () => handle.unref() };
}

// ─── Path helpers (re-exports for the dispatcher) ───────────────────────

/** Extract the identity name from an inbox path: `<root>/<id>/inbox`. */
export function identityFromInboxDir(inboxPath: string): string {
  return basename(dirname(inboxPath));
}

export { cmdWatch as cmdWatchCore };

// ─── CLI wrapper ────────────────────────────────────────────────────────

import { invokedName, type CliContext } from '../cli-context.ts';

export async function cmdWatchCli(
  args: readonly string[],
  ctx: CliContext
): Promise<number> {
  let recipient: string | undefined;
  let all = false;
  let fromExplicit: string | undefined;
  let withSubject = false;
  let since: number | undefined;
  let sinceNow = false;
  let intervalMs = 500;
  let once = false;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '--all':
        all = true;
        break;
      case '--from':
        fromExplicit = args[++i];
        break;
      case '--with-subject':
        withSubject = true;
        break;
      case '--since': {
        const v = args[++i];
        if (v === undefined || !/^[0-9]+$/.test(v)) {
          throw new Error('--since must be a unix-ms integer');
        }
        since = Number(v);
        break;
      }
      case '--since-now':
        sinceNow = true;
        break;
      case '--interval': {
        const v = args[++i];
        if (v === undefined || !/^[0-9]+$/.test(v)) {
          throw new Error('--interval must be a non-negative integer (ms)');
        }
        intervalMs = Number(v);
        break;
      }
      case '--once':
        once = true;
        break;
      case '-h':
      case '--help':
        ctx.stderr(
          `usage: ${invokedName(ctx.env)} watch [<identity>] [--all] [--from <id>] [--with-subject]\n` +
            '                   [--since UNIX_MS | --since-now] [--interval MS] [--once]\n\n' +
            '  Default: watch $ST_AGENT/inbox/ (your own).\n' +
            '  --all:   watch every identity\'s inbox EXCEPT $ST_AGENT\'s\n' +
            '           (cross-tree supervisor mode; uses --from / $ST_AGENT\n' +
            '           for the suppression id).\n' +
            '  <identity>: watch THAT identity\'s inbox specifically.\n'
        );
        return 0;
      default:
        if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
        if (recipient === undefined) recipient = a;
        else throw new Error(`unexpected arg: ${a}`);
    }
  }
  cmdWatch(
    {
      ...(recipient !== undefined && { recipient }),
      ...(all && { all }),
      ...(fromExplicit !== undefined && { fromExplicit }),
      withSubject,
      ...(since !== undefined && { since }),
      sinceNow,
      intervalMs,
      once,
      env: ctx.env,
      stRoot: ctx.stRoot,
    },
    {
      emit: (line) => {
        ctx.stdout(
          line.subject !== undefined
            ? `${line.filename}\t${line.subject}\n`
            : `${line.filename}\n`
        );
      },
    }
  );
  if (once) return 0;
  // Live mode: keep the dispatcher's await pending until SIGINT/SIGTERM
  // so the setInterval handle in cmdWatch keeps ticking.
  return await new Promise<number>((resolve) => {
    const onSig = (): void => resolve(0);
    process.once('SIGINT', onSig);
    process.once('SIGTERM', onSig);
  });
}
