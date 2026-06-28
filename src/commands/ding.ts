// commands/ding.ts — busy-aware push notifier for harnesses without
// extension points. Watches `<identity>/inbox/`, reads
// `<identity>/status`, and pty-sends a notice into a target session
// only when the agent is `available` or `offline`. Buffers while
// `busy`/`dnd`, flushes when status flips back.
//
// Long-running. Lives in the same process as `coord ding ...`; pair
// with `pty up` (or any supervisor) for restart-on-crash. Designed
// so the underlying daemon (`runDing`) is testable without a real
// pty binary or a real Coord — see tests/unit/ding.test.ts.

import { spawn } from 'node:child_process';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

import type { CliContext } from '../cli-context.ts';
import {
  inboxDir,
  statusPath,
  STATUS_REFRESH_MS,
  TIDY_CHECK_INTERVAL_MS,
  validFilename,
} from '../common.ts';
import { type Coord } from '../lib.ts';
import { refreshIdentityStatus } from '../commands/status.ts';
import { evaluateDrift, type DriftResult } from '../mcp/tidy-check.ts';
import {
  asFilename,
  type Filename,
  type Identity,
  type State,
  type WatchEvent,
} from '../types.ts';

const DEFAULT_INTERVAL_MS = 1000;

/** brief-031 amendment: how often to check whether the target pty
 *  session is still alive. 30s is more than fast enough — the
 *  expensive case is an orphan daemon hanging around for hours after
 *  the agent died, not the few seconds between session-death and
 *  ding-exit. */
const DEFAULT_SESSION_WATCH_INTERVAL_MS = 30_000;

const SUPPRESS_STATES: ReadonlySet<State> = new Set<State>(['busy', 'dnd']);

// brief-031: tidy-check gate is stricter than the inbox-arrival gate.
// `unknown` joins busy/dnd because we don't know what the agent's
// actually doing — same call as the MCP tick made in brief-030.
const TIDY_GATE_STATES: ReadonlySet<State> = new Set<State>([
  'busy',
  'dnd',
  'unknown',
]);

/** Test seam: how the daemon delivers a notice. Production binds to
 * `pty send <session> --with-delay 0.5 --seq <text> --seq key:return`.
 * The `--with-delay 0.5` (brief-034) keeps the terminal from racing
 * the trailing Enter against the text on bracketed-paste-aware
 * input panes. */
export interface PtySender {
  (
    sessionName: string,
    sequences: readonly string[]
  ): Promise<{ status: number; stderr: string }>;
}

/** Test seam: how the daemon checks whether the target session is
 *  alive. Production reads `<PTY_SESSION_DIR>/<session>.pid` and
 *  probes the PID with `process.kill(pid, 0)`. */
export interface IsSessionAlive {
  (sessionName: string): boolean;
}

export interface DingDeps {
  /** Pre-built Coord. Production uses `createCoord({ root, identity })`. */
  coord: Coord;
  /** Identity whose inbox + status the daemon watches. */
  identity: Identity;
  /** Target pty session name (matches `pty list`). */
  ptySession: string;
  /** How often to re-check status when buffered notices are pending. */
  intervalMs?: number;
  /**
   * brief-031: how often to run the tidy-check drift detector and
   * pty-send a summary if drift fires. Defaults to
   * TIDY_CHECK_INTERVAL_MS (20 min). Set to 0 to disable tidy-check
   * entirely (the daemon becomes push-only, the pre-brief-031
   * behavior). Tests pass a small value to observe ticks.
   */
  tidyIntervalMs?: number;
  /** Optional test-injectable sender. Defaults to the real `pty` binary. */
  ptySend?: PtySender;
  /**
   * brief-031: test seam for the tidy-check clock. Production omits
   * → Date.now. The unit suite injects to deterministically advance
   * drift age without sleeping real minutes.
   */
  tidyNow?: () => number;
  /**
   * brief-031 amendment: when true (default), ding periodically
   * checks whether the target pty session is still alive and exits
   * cleanly when it's not. Disable with the
   * `--no-exit-when-session-gone` CLI flag for the rare case where
   * you want ding to wait for the session to come back.
   */
  exitWhenSessionGone?: boolean;
  /**
   * brief-031 amendment: how often to run the session-alive check.
   * Defaults to DEFAULT_SESSION_WATCH_INTERVAL_MS (30s). Tests use a
   * small value to observe transitions without sleeping. Ignored
   * when exitWhenSessionGone is false.
   */
  sessionWatchIntervalMs?: number;
  /**
   * brief-031 amendment: test seam for the alive check. Defaults to
   * a pid-file + process.kill(pid, 0) probe under
   * $PTY_SESSION_DIR.
   */
  isSessionAlive?: IsSessionAlive;
  /**
   * brief-032: how often (ms) to refresh the watched identity's
   * status file mtime. Mirrors the MCP server's brief-023 behavior
   * so Codex agents (no per-identity MCP server) don't drift into
   * `unknown` over long inactivity. Defaults to STATUS_REFRESH_MS
   * (5 min). Set to 0 to disable.
   */
  statusRefreshIntervalMs?: number;
  /** Stops the daemon. Aborts the watcher and clears the status timer. */
  signal?: AbortSignal;
  /** Where to log warnings. Defaults to `process.stderr.write`. */
  stderr?: (s: string) => void;
}

interface BufferedEvent {
  filename: Filename;
  from: Identity | '';
  subject?: string;
}

/**
 * Run the ding daemon. Resolves when the AbortSignal aborts (or
 * when the upstream watcher exits, which only happens on signal in
 * normal operation). Production callers from `cmdDingCli` expect
 * this to run forever; tests pass a tight signal.
 */
export async function runDing(deps: DingDeps): Promise<void> {
  const intervalMs = deps.intervalMs ?? DEFAULT_INTERVAL_MS;
  const send = deps.ptySend ?? defaultPtySend;
  const log = deps.stderr ?? ((s) => process.stderr.write(s));

  // brief-031 amendment: an internal AbortController so the
  // session-watch tick can end runDing on its own (target session
  // died → cleanly exit) without process.exit. The caller's
  // deps.signal still drives external aborts; we just chain it in.
  const internalAc = new AbortController();
  if (deps.signal !== undefined) {
    if (deps.signal.aborted) internalAc.abort();
    else
      deps.signal.addEventListener('abort', () => internalAc.abort(), {
        once: true,
      });
  }
  const signal = internalAc.signal;

  const buffer: BufferedEvent[] = [];
  let timer: ReturnType<typeof setInterval> | undefined;

  function ensureTimerArmed(): void {
    if (timer !== undefined) return;
    timer = setInterval(() => {
      // Schedule the flush; ignore the returned promise (errors
      // are surfaced via stderr inside `tryFlush`).
      void tryFlush();
    }, intervalMs);
  }

  function disarmTimer(): void {
    if (timer !== undefined) {
      clearInterval(timer);
      timer = undefined;
    }
  }

  // brief-031: tidy-check tick. Independent of the inbox-arrival
  // buffer above; runs on its own interval, reads identity status,
  // gates on busy/dnd/unknown, evaluates drift, dedupes per-condition,
  // pty-sends a single-line summary when a new condition appears.
  const tidyIntervalMs = deps.tidyIntervalMs ?? TIDY_CHECK_INTERVAL_MS;
  let tidyTimer: ReturnType<typeof setInterval> | undefined;
  let lastTidyFired = { inbox: false, doingTask: false, journal: false };
  async function runTidyTick(): Promise<void> {
    let state: State;
    try {
      state = await deps.coord.getStatus(deps.identity);
    } catch (err) {
      log(`coord ding: tidy getStatus failed: ${errMsg(err)}\n`);
      return; // best-effort; don't arm dedup on errors
    }
    // Gate: busy/dnd/unknown → no emit, no lastFired update. Drift
    // accumulates; next eligible tick catches up.
    if (TIDY_GATE_STATES.has(state)) return;
    let drift: DriftResult;
    try {
      const driftOpts: { now?: () => number } = {};
      if (deps.tidyNow !== undefined) driftOpts.now = deps.tidyNow;
      drift = evaluateDrift(deps.identity, deps.coord.root, driftOpts);
    } catch (err) {
      log(`coord ding: tidy evaluate failed: ${errMsg(err)}\n`);
      return;
    }
    const newCondition =
      (drift.inbox && !lastTidyFired.inbox) ||
      (drift.doingTask && !lastTidyFired.doingTask) ||
      (drift.journal && !lastTidyFired.journal);
    if (newCondition && drift.body.length > 0) {
      const text = formatTidyLine(drift);
      let result: { status: number; stderr: string };
      try {
        result = await send(deps.ptySession, [text, 'key:return']);
      } catch (err) {
        log(`coord ding: tidy pty send failed: ${errMsg(err)}\n`);
        // Don't arm lastFired — we want a retry on next tick.
        return;
      }
      if (result.status !== 0) {
        const tail = result.stderr.trim().slice(-200);
        log(
          `coord ding: tidy pty send to "${deps.ptySession}" exited ${result.status}${
            tail ? `: ${tail}` : ''
          }\n`
        );
        return; // same — leave lastFired alone for retry
      }
    }
    // Update lastFired on every eligible tick (not just emits) so a
    // drift that clears stops counting as "old news" — only its
    // recurrence-after-clear re-fires.
    lastTidyFired = {
      inbox: drift.inbox,
      doingTask: drift.doingTask,
      journal: drift.journal,
    };
  }
  function startTidyTick(): void {
    if (tidyIntervalMs <= 0) return;
    tidyTimer = setInterval(() => {
      void runTidyTick();
    }, tidyIntervalMs);
    tidyTimer.unref?.();
  }
  function stopTidyTick(): void {
    if (tidyTimer !== undefined) {
      clearInterval(tidyTimer);
      tidyTimer = undefined;
    }
  }

  // brief-031 amendment: session-watch tick. When the target pty
  // session is gone, abort the internal signal so runDing's
  // for-await falls through to the finally block and the daemon
  // exits cleanly. Default ON; opt-out via `--no-exit-when-session-gone`.
  const exitWhenSessionGone = deps.exitWhenSessionGone !== false;
  const sessionWatchIntervalMs =
    deps.sessionWatchIntervalMs ?? DEFAULT_SESSION_WATCH_INTERVAL_MS;
  const isSessionAlive = deps.isSessionAlive ?? defaultIsSessionAlive;
  let sessionWatchTimer: ReturnType<typeof setInterval> | undefined;
  function runSessionWatchTick(): void {
    let alive: boolean;
    try {
      alive = isSessionAlive(deps.ptySession);
    } catch (err) {
      // Probe failure: be conservative — treat as alive so we don't
      // tear down on a transient permission glitch. Log so the
      // operator can investigate.
      log(`coord ding: session-alive check failed: ${errMsg(err)}\n`);
      return;
    }
    if (!alive) {
      log(
        `coord ding: target session "${deps.ptySession}" is gone; exiting.\n`
      );
      internalAc.abort();
    }
  }
  function startSessionWatch(): void {
    if (!exitWhenSessionGone) return;
    if (sessionWatchIntervalMs <= 0) return;
    sessionWatchTimer = setInterval(
      runSessionWatchTick,
      sessionWatchIntervalMs
    );
    sessionWatchTimer.unref?.();
  }
  function stopSessionWatch(): void {
    if (sessionWatchTimer !== undefined) {
      clearInterval(sessionWatchTimer);
      sessionWatchTimer = undefined;
    }
  }

  // brief-032: status-file mtime refresh tick. Mirrors the MCP
  // server's brief-023 behavior so Codex agents (which have no
  // per-identity MCP server) don't drift into the `unknown` staleness
  // window. Delegates to the same helper the MCP path uses; the only
  // logging difference is ding's stderr surface for `error` outcomes.
  const statusRefreshIntervalMs =
    deps.statusRefreshIntervalMs ?? STATUS_REFRESH_MS;
  let statusRefreshTimer: ReturnType<typeof setInterval> | undefined;
  function runStatusRefreshTick(): void {
    const outcome = refreshIdentityStatus(deps.identity, deps.coord.root);
    if (outcome === 'error') {
      log(
        `coord ding: status refresh for "${deps.identity}" failed (best-effort, will retry next tick).\n`
      );
    } else if (outcome === 'left-corrupt') {
      log(
        `coord ding: status file for "${deps.identity}" contains invalid content; refresh skipped.\n`
      );
    }
    // refreshed / wrote-default / left-unknown are silent — they're
    // either the happy path or a deliberate no-op.
  }
  function startStatusRefresh(): void {
    if (statusRefreshIntervalMs <= 0) return;
    statusRefreshTimer = setInterval(
      runStatusRefreshTick,
      statusRefreshIntervalMs
    );
    statusRefreshTimer.unref?.();
  }
  function stopStatusRefresh(): void {
    if (statusRefreshTimer !== undefined) {
      clearInterval(statusRefreshTimer);
      statusRefreshTimer = undefined;
    }
  }

  async function tryFlush(): Promise<void> {
    if (buffer.length === 0) {
      disarmTimer();
      return;
    }
    let state: State;
    try {
      state = await deps.coord.getStatus(deps.identity);
    } catch (err) {
      log(
        `coord ding: getStatus failed: ${errMsg(err)}\n`
      );
      return;
    }
    if (SUPPRESS_STATES.has(state)) {
      // Still busy — keep the timer armed.
      return;
    }
    // Drain in chronological insertion order.
    while (buffer.length > 0) {
      const ev = buffer.shift()!;
      await deliver(send, deps.ptySession, ev, log);
    }
    disarmTimer();
  }

  async function onEvent(filename: Filename): Promise<void> {
    let state: State;
    try {
      state = await deps.coord.getStatus(deps.identity);
    } catch (err) {
      log(`coord ding: getStatus failed: ${errMsg(err)}\n`);
      // If we can't read status, lean toward delivering — better
      // than silently dropping a coord message.
      state = 'available';
    }
    let event: BufferedEvent;
    try {
      event = await buildEvent(deps.coord, deps.identity, filename);
    } catch (err) {
      log(`coord ding: read failed for ${filename}: ${errMsg(err)}\n`);
      return;
    }
    if (SUPPRESS_STATES.has(state)) {
      buffer.push(event);
      ensureTimerArmed();
      return;
    }
    await deliver(send, deps.ptySession, event, log);
  }

  // brief-035 t2: ding scan-on-startup. On boot, replay any inbox
  // files whose mtime is newer than the watched identity's status
  // mtime through the same onEvent path the watcher uses. This makes
  // ding self-healing across restarts — a message that arrived while
  // the old ding was down (or before a binary upgrade) doesn't sit
  // un-pushed waiting for the next live arrival. Status mtime is the
  // "I've already addressed everything up to this point" marker:
  // files older than that are considered handled. busy/dnd gating
  // still applies via onEvent's existing branch.
  async function scanStartupBacklog(): Promise<void> {
    let statusMtimeMs = 0;
    try {
      statusMtimeMs = statSync(
        statusPath(deps.identity, deps.coord.root)
      ).mtimeMs;
    } catch {
      // missing or unreadable status file → treat as 0 (all inbox files
      // are eligible). A fresh agent that never set status still gets
      // the backlog replayed.
    }
    const dir = inboxDir(deps.identity, deps.coord.root);
    let entries: string[];
    try {
      entries = readdirSync(dir);
    } catch {
      return; // no inbox dir or unreadable
    }
    const eligible: { filename: string; mtimeMs: number }[] = [];
    for (const name of entries) {
      if (!validFilename(name)) continue;
      let st: ReturnType<typeof statSync>;
      try {
        st = statSync(join(dir, name));
      } catch {
        continue;
      }
      if (st.mtimeMs > statusMtimeMs) {
        eligible.push({ filename: name, mtimeMs: st.mtimeMs });
      }
    }
    // Chronological by filename (the <unix-ms> prefix sorts correctly
    // lexicographically up to year 5138).
    eligible.sort((a, b) => a.filename.localeCompare(b.filename));
    for (const { filename } of eligible) {
      await onEvent(asFilename(filename));
    }
  }

  // Arm the brief-031 tidy-check tick alongside the inbox watcher.
  // It runs on its own setInterval — independent of the
  // buffer-flush timer above — and the AbortSignal that ends the
  // watcher also ends ding's process lifetime, at which point the
  // finally below will clear the tidy timer too.
  startTidyTick();

  // brief-031 amendment: also arm the session-watch tick. When the
  // target session goes away, this aborts the internal signal,
  // which ends the for-await below and falls through to the finally.
  startSessionWatch();

  // brief-032: arm the status-file mtime refresh tick.
  startStatusRefresh();

  // brief-035 t2: replay any inbox backlog BEFORE the watcher's
  // sinceNow:true filter starts, so messages that landed while ding
  // was down (or before a binary upgrade) still get pushed. Files
  // arriving DURING the scan are out of luck (not in the readdir
  // snapshot, and watcher hasn't armed yet) — but the next live
  // arrival or tidy-check tick will surface the drift.
  await scanStartupBacklog();

  // Drive the watcher. coord.watch returns an AsyncIterable; the loop
  // exits when the AbortSignal aborts.
  try {
    const watchOpts: Parameters<Coord['watch']>[1] = {
      withSubject: true,
      sinceNow: true,
    };
    watchOpts.signal = signal;
    for await (const ev of deps.coord.watch(
      deps.identity,
      watchOpts
    ) as AsyncIterable<WatchEvent>) {
      if (ev.folder !== 'inbox') continue;
      await onEvent(ev.filename);
    }
  } finally {
    disarmTimer();
    stopTidyTick();
    stopSessionWatch();
    stopStatusRefresh();
  }
}

/** Default session-alive probe: pid file at
 *  `${PTY_SESSION_DIR ?? ~/.local/state/pty}/${sessionName}.pid`,
 *  then `process.kill(pid, 0)` to check the PID. Any read error or
 *  ESRCH from the kill probe → false (session gone).
 *
 *  Conservative on weird states: a pid file present with an unparseable
 *  PID, or a PID whose probe throws for any reason → false. Easier to
 *  restart ding than to defend every edge. */
const defaultIsSessionAlive: IsSessionAlive = (sessionName) => {
  const dir =
    process.env.PTY_SESSION_DIR ?? join(homedir(), '.local', 'state', 'pty');
  const pidFile = join(dir, `${sessionName}.pid`);
  let raw: string;
  try {
    raw = readFileSync(pidFile, 'utf8');
  } catch {
    return false;
  }
  const pid = Number(raw.trim());
  if (!Number.isFinite(pid) || pid <= 0) return false;
  try {
    // signal 0: existence + permission check, no actual signal sent
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};

/**
 * Single-line summary suitable for pty-send into a terminal session.
 * Distinct from the MCP frame body (which is multi-line markdown) —
 * Codex sees this as one line of typed input, so we keep it scannable.
 */
function formatTidyLine(drift: DriftResult): string {
  const parts: string[] = [];
  if (drift.inbox) {
    parts.push(
      `inbox=${drift.detail.inboxStaleCount} (oldest ${formatAge(drift.detail.oldestInboxAgeMs)})`
    );
  }
  if (drift.journal) {
    parts.push(
      `no journal entry for ${formatAge(drift.detail.journalLagMs)}`
    );
  }
  return `coord tidy-check: ${parts.join('; ')}.`;
}

function formatAge(ms: number): string {
  if (ms < 60_000) return `${Math.round(ms / 1000)}s`;
  if (ms < 60 * 60_000) return `${Math.round(ms / 60_000)}m`;
  if (ms < 24 * 60 * 60_000) return `${Math.round(ms / (60 * 60_000))}h`;
  return `${Math.round(ms / (24 * 60 * 60_000))}d`;
}

async function buildEvent(
  coord: Coord,
  identity: Identity,
  filename: Filename
): Promise<BufferedEvent> {
  // Read the file to extract `from` for the notice. Errors propagate
  // to the caller (which logs + drops the event).
  const r = await coord.read(identity, filename);
  return {
    filename,
    from: r.message.from,
    ...(r.message.subject !== undefined && { subject: r.message.subject }),
  };
}

function buildSequences(ev: BufferedEvent): string[] {
  const subject = ev.subject ?? '(no subject)';
  const from = ev.from === '' ? 'unknown' : ev.from;
  const text = `you have a new coord message: ${subject} (from ${from}); check your inbox`;
  return [text, 'key:return'];
}

async function deliver(
  send: PtySender,
  sessionName: string,
  ev: BufferedEvent,
  log: (s: string) => void
): Promise<void> {
  const sequences = buildSequences(ev);
  let result: { status: number; stderr: string };
  try {
    result = await send(sessionName, sequences);
  } catch (err) {
    log(`coord ding: pty send failed: ${errMsg(err)}\n`);
    return;
  }
  if (result.status !== 0) {
    const tail = result.stderr.trim().slice(-200);
    log(
      `coord ding: pty send to "${sessionName}" exited ${result.status}${
        tail ? `: ${tail}` : ''
      }\n`
    );
  }
}

/**
 * brief-034: build the argv passed to `pty send`. Inserts
 * `--with-delay 0.5` between the session name and the --seq pairs so
 * the terminal commits the text payload before processing the
 * trailing `key:return`. Without the delay, agents using
 * bracketed-paste mode (e.g. Codex's TUI input pane) can see the
 * Enter race the text — the notice appears in the prompt but never
 * submits as a turn. Exported so tests can pin the wire shape
 * without spawning a real `pty` subprocess.
 */
export function buildPtySendArgs(
  sessionName: string,
  sequences: readonly string[]
): string[] {
  const args = ['send', sessionName, '--with-delay', '0.5'];
  for (const s of sequences) {
    args.push('--seq', s);
  }
  return args;
}

const defaultPtySend: PtySender = (sessionName, sequences) =>
  new Promise((resolve) => {
    const args = buildPtySendArgs(sessionName, sequences);
    let proc: ReturnType<typeof spawn>;
    try {
      proc = spawn('pty', args, {
        stdio: ['ignore', 'ignore', 'pipe'],
      });
    } catch (err) {
      resolve({ status: -1, stderr: errMsg(err) });
      return;
    }
    let stderr = '';
    proc.stderr?.on('data', (chunk: Buffer) => {
      stderr += chunk.toString('utf8');
    });
    proc.once('error', (err) => {
      resolve({ status: -1, stderr: err.message });
    });
    proc.once('close', (status) => {
      resolve({ status: status ?? -1, stderr });
    });
  });

function errMsg(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

// ─── CLI wrapper ────────────────────────────────────────────────────────

export async function cmdDingCli(
  args: readonly string[],
  ctx: CliContext
): Promise<number> {
  let ptySession: string | undefined;
  let identityArg: string | undefined;
  let intervalMs: number | undefined;
  let tidyIntervalMs: number | undefined;
  let statusRefreshIntervalMs: number | undefined;
  // brief-031 amendment: default ON. CLI flag flips to false.
  let exitWhenSessionGone = true;

  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '-h':
      case '--help':
        ctx.stderr(
          'usage: coord ding <pty-session> [--identity ID] [--interval MS]\n' +
            '                          [--tidy-interval-ms MS]\n' +
            '                          [--status-refresh-interval-ms MS]\n' +
            '                          [--no-exit-when-session-gone]\n\n' +
            '  Watches <identity>/inbox/ and pty-sends a notice into\n' +
            '  <pty-session> on every new arrival. Buffers while status\n' +
            '  is busy/dnd; flushes when status flips back to available.\n' +
            '  Also runs a periodic tidy-check that pty-sends a drift\n' +
            '  summary when inbox/tasks/journal are out of date, AND\n' +
            "  refreshes the watched identity's status file mtime so\n" +
            "  the identity doesn't fall into `unknown` over long\n" +
            "  inactivity (mirrors the MCP server's brief-023 behavior\n" +
            "  for Codex agents that don't run an MCP server per identity).\n" +
            '  Exits cleanly when the target pty session is gone.\n' +
            '  Long-running — pair with `pty up` for supervision.\n\n' +
            '  --identity ID                    Coord identity to watch. Defaults to $COORD_IDENTITY.\n' +
            '  --interval MS                    Status poll interval while buffered. Default 1000ms.\n' +
            '  --tidy-interval-ms MS            Tidy-check tick interval. Default 20 min.\n' +
            '                                   Set to 0 to disable tidy-check entirely\n' +
            '                                   (push-only mode, pre-brief-031 behavior).\n' +
            '  --status-refresh-interval-ms MS  Status mtime refresh interval. Default 5 min.\n' +
            '                                   Set to 0 to disable.\n' +
            '  --exit-when-session-gone         Exit when the target pty session is gone (default).\n' +
            '  --no-exit-when-session-gone      Keep running even when the target session\n' +
            '                                   is gone (rare; opt-out).\n'
        );
        return 0;
      case '--identity':
        identityArg = args[++i];
        break;
      case '--interval': {
        const v = args[++i];
        if (v === undefined || !/^[0-9]+$/.test(v)) {
          throw new Error('--interval must be a positive integer (ms)');
        }
        intervalMs = Number(v);
        break;
      }
      case '--tidy-interval-ms': {
        const v = args[++i];
        if (v === undefined || !/^[0-9]+$/.test(v)) {
          throw new Error(
            '--tidy-interval-ms must be a non-negative integer (ms); 0 disables'
          );
        }
        tidyIntervalMs = Number(v);
        break;
      }
      case '--status-refresh-interval-ms': {
        const v = args[++i];
        if (v === undefined || !/^[0-9]+$/.test(v)) {
          throw new Error(
            '--status-refresh-interval-ms must be a non-negative integer (ms); 0 disables'
          );
        }
        statusRefreshIntervalMs = Number(v);
        break;
      }
      case '--exit-when-session-gone':
        exitWhenSessionGone = true;
        break;
      case '--no-exit-when-session-gone':
        exitWhenSessionGone = false;
        break;
      default:
        if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
        if (ptySession === undefined) {
          ptySession = a;
        } else {
          throw new Error(`unexpected positional arg: ${a}`);
        }
    }
  }

  if (ptySession === undefined) {
    throw new Error('coord ding requires a <pty-session> name');
  }

  const root = ctx.coordRoot;
  if (!root) {
    throw new Error('COORD_ROOT must be set for `coord ding`');
  }
  const identityValue = identityArg ?? ctx.env.COORD_IDENTITY;
  if (!identityValue) {
    throw new Error(
      '`coord ding` needs --identity ID or $COORD_IDENTITY to know which inbox to watch'
    );
  }

  // Lazy-import the embeddable factory + asIdentity so non-ding
  // invocations don't pull lib.ts into the dispatcher hot path.
  const { createCoord } = await import('../lib.ts');
  const { asIdentity } = await import('../types.ts');

  const identity = asIdentity(identityValue);
  const coord = createCoord({ root, identity });

  const ac = new AbortController();
  const onSig = (): void => ac.abort();
  process.once('SIGINT', onSig);
  process.once('SIGTERM', onSig);

  try {
    await runDing({
      coord,
      identity,
      ptySession,
      ...(intervalMs !== undefined && { intervalMs }),
      ...(tidyIntervalMs !== undefined && { tidyIntervalMs }),
      ...(statusRefreshIntervalMs !== undefined && {
        statusRefreshIntervalMs,
      }),
      exitWhenSessionGone,
      signal: ac.signal,
      stderr: ctx.stderr,
    });
  } finally {
    process.removeListener('SIGINT', onSig);
    process.removeListener('SIGTERM', onSig);
  }
  return 0;
}
