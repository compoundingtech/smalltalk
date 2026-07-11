// commands/ding.ts — busy-aware push notifier for harnesses without
// extension points. Watches `<identity>/inbox/`, reads
// `<identity>/status`, and pty-sends a notice into a target session
// only when the agent is `available` or `offline`. Buffers while
// `busy`/`dnd`, flushes when status flips back.
//
// Long-running. Lives in the same process as `st ding ...`; pair
// with `pty up` (or any supervisor) for restart-on-crash. Designed
// so the underlying daemon (`runDing`) is testable without a real
// pty binary or a real St — see tests/unit/ding.test.ts.

import { spawn, spawnSync } from 'node:child_process';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

import { invokedName, type CliContext } from '../cli-context.ts';
import {
  inboxDir,
  msNow,
  statusPath,
  STATUS_REFRESH_MS,
  TIDY_CHECK_INTERVAL_MS,
  validFilename,
} from '../common.ts';
import { type St } from '../lib.ts';
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

/**
 * Session-flap debounce: how many CONSECUTIVE "target gone"
 * observations the session-watch tick needs before tripping the
 * exit-when-gone path. A pty `--permanent` session is auto-restarted
 * by pty's supervisor; the window between the old process's exit
 * and the pidfile's rewrite can look "gone" to `process.kill(pid,
 * 0)` for a tick or two. Without debounce, ding exits right when
 * its target is about to come back; its own supervisor restarts
 * ding eventually but arrivals during the gap are missed. Three
 * consecutive misses at the default 30s interval = ~90s of "really
 * gone" evidence before we trip; at aggressive test intervals a
 * quick flap (single miss) rides through cleanly.
 */
const SESSION_GONE_DEBOUNCE_MISSES = 3;

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

/** Test seam: how the typing-aware guard reads the target pane.
 *  Production binds to `pty peek --plain <session>` (plain text, no
 *  ANSI). Used to frame-diff the pane before a poke — see
 *  {@link isPaneBusy}. */
export interface PtyPeeker {
  (
    sessionName: string
  ): Promise<{ status: number; stdout: string; stderr: string }>;
}

/** Test seam: bracketed-paste text into the target pane WITHOUT
 *  submitting. Production binds to `pty send <session> --paste <text>`.
 *  Used by the walked-away-mid-type path to preserve a human's
 *  in-progress input (append a newline + the ding, then submit) so no
 *  typed text is clobbered. See {@link preserveDeliver}. */
export interface PtyPaster {
  (
    sessionName: string,
    text: string
  ): Promise<{ status: number; stderr: string }>;
}

/** Test seam: how the daemon checks whether the target session is
 *  alive. Production reads `<PTY_SESSION_DIR>/<session>.pid` and
 *  probes the PID with `process.kill(pid, 0)`. */
export interface IsSessionAlive {
  (sessionName: string): boolean;
}

export interface DingDeps {
  /** Pre-built St. Production uses `createSt({ root, identity })`. */
  st: St;
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
  /**
   * Reboot self-healing: how often (ms) to re-scan the inbox for
   * files that are unarchived + not-recently-delivered and re-poke
   * the target session. Defaults to DEFAULT_RESCAN_INTERVAL_MS
   * (60s). Set to 0 to disable (the pre-tick push-only behavior).
   * Tests use a small value to observe ticks.
   */
  rescanIntervalMs?: number;
  /**
   * Quiet period (ms) after a successful delivery before the
   * re-scan will re-poke the same file. Defaults to
   * DEFAULT_RESCAN_QUIET_AFTER_DELIVERY_MS (90s). Tunes how long
   * the agent has to archive a delivered message before ding
   * assumes it may have been missed and re-nudges.
   */
  rescanQuietAfterDeliveryMs?: number;
  /**
   * brief-036 (typing-aware ding): test seam for reading the target
   * pane. Production binds to `pty peek --plain <session>`. When the
   * guard is enabled and a message isn't urgent, the daemon peeks the
   * pane before poking; if it's active (frames differ, or the input
   * line has un-submitted text) it holds and retries rather than
   * interrupting a mid-type human.
   */
  ptyPeek?: PtyPeeker;
  /**
   * brief-036: master toggle for the typing-aware guard. Default true.
   * When false the daemon delivers on arrival (the pre-brief-036
   * behavior). Env: `ST_DING_PANE_GUARD=0`.
   */
  paneGuard?: boolean;
  /**
   * brief-036: gap (ms) between the two `pty peek` frames used to
   * detect activity. Default {@link DEFAULT_PEEK_DIFF_MS} (300ms).
   * Env: `ST_DING_PEEK_DIFF_MS`.
   */
  peekDiffMs?: number;
  /**
   * brief-036: how long (ms) to hold a busy pane before re-checking.
   * Default {@link DEFAULT_HOLD_RETRY_MS} (20s). Env:
   * `ST_DING_HOLD_RETRY_MS`.
   */
  holdRetryMs?: number;
  /**
   * brief-036: max times to hold for a busy pane before force-
   * delivering anyway (never drop). Default {@link DEFAULT_MAX_HOLDS}
   * (3) → ~60s worst-case hold. Env: `ST_DING_MAX_HOLDS`.
   */
  maxHolds?: number;
  /**
   * brief-036: also treat the pane as busy when its last non-blank
   * line looks like a prompt with un-submitted text (the "typed then
   * paused" case that frame-diff alone misses). Best-effort +
   * per-harness-tunable. Default true; env `ST_DING_INPUT_GUARD=0`.
   */
  inputGuard?: boolean;
  /**
   * brief-036: pattern matched against the pane's last non-blank line
   * for the input-area check. Default {@link DEFAULT_INPUT_PATTERN}
   * (a prompt glyph followed by text). Env: `ST_DING_INPUT_PATTERN`
   * (a regex source string).
   */
  inputPattern?: RegExp;
  /**
   * brief-036 refinement: consecutive unchanged-input retries before a
   * non-empty input line is treated as walked-away-mid-type and
   * preserve-delivered. Default {@link DEFAULT_INPUT_STALE_MAX} (3).
   * Env: `ST_DING_INPUT_STALE_MAX`.
   */
  inputStaleMax?: number;
  /**
   * brief-036 refinement: test seam for the bracketed-paste primitive
   * used by preserve-and-deliver. Production binds to
   * `pty send <session> --paste <text>`.
   */
  ptyPaste?: PtyPaster;
  /**
   * When true, emit verbose `[st ding debug]` lines to stderr:
   *   - per-rescan-tick summary (inbox / in-flight / quiet-skipped
   *     / attempted counts)
   *   - per-delivery-attempt (filename, session name, exit status,
   *     stderr tail)
   *   - startup-backlog scan summary (files eligible / skipped)
   * Used by evals + operators to diagnose delivery/rescan gaps
   * without pty-peeking. Toggled by ST_DING_DEBUG=1 in
   * `cmdDingCli`. Default off — production doesn't need the noise.
   */
  debug?: boolean;
  /** Stops the daemon. Aborts the watcher and clears the status timer. */
  signal?: AbortSignal;
  /** Where to log warnings. Defaults to `process.stderr.write`. */
  stderr?: (s: string) => void;
}

interface BufferedEvent {
  filename: Filename;
  from: Identity | '';
  subject?: string;
  /**
   * Retry counter — bumped when `deliver()` returns a failure signal
   * and the event gets requeued. Capped by {@link MAX_DELIVER_RETRIES}
   * so a permanently-broken target doesn't produce an infinite retry
   * loop. Undefined = first attempt.
   */
  retries?: number;
  /**
   * brief-036: how many times this event has been HELD for a busy
   * pane (typing-aware guard). Distinct from {@link retries} (transient
   * pty failures). At `maxHolds` we force-deliver — a held message is
   * never dropped. Undefined = never held.
   */
  holds?: number;
  /**
   * brief-036: don't re-attempt a held event before this `msNow()`
   * value. Set to `now + holdRetryMs` on each hold so the ~20s retry
   * cadence rides the existing 1s flush timer without a second timer.
   */
  notBefore?: number;
  /**
   * brief-036: message priority from frontmatter. `high` = urgent →
   * skip the pane guard and deliver immediately.
   */
  priority?: string;
  /**
   * brief-036 refinement: the pane's input-line text observed on the
   * previous hold, used to tell "actively typing" (text changes across
   * retries) from "walked away mid-type" (text stays UNCHANGED). Only
   * set while an un-submitted input line is present.
   */
  lastInputText?: string;
  /**
   * brief-036 refinement: consecutive retries the input line has been
   * present AND unchanged. At `inputStaleMax` we treat it as
   * walked-away and preserve-and-deliver (newlines + ding + submit)
   * rather than holding forever or clobbering the typed text.
   */
  inputStaleCount?: number;
}

/**
 * Max retries per event before giving up (with a loud log). Bounded so
 * a permanently-broken pty target doesn't monopolize the flush loop
 * forever, but generous enough to survive a session flap +
 * restart-in-under-a-minute (each retry runs on the flush interval,
 * so 5 retries = ~5s at the default 1s interval).
 */
const MAX_DELIVER_RETRIES = 5;

/**
 * How long after the daemon starts to keep the startup-dedup set
 * populated. Beyond this window the scan+watcher race is over — any
 * arrival goes through the watcher, and the dedup set only wastes
 * memory. 60s is more than enough for a fresh filesystem watcher to
 * settle; a slow FSEvents subscription typically arms in single-digit
 * ms. Cleared when the window expires.
 */
const STARTUP_DEDUP_WINDOW_MS = 60_000;

/**
 * How often the periodic backlog re-scan tick fires. Reads the
 * inbox and re-pokes for any file that's unarchived and hasn't
 * been delivered recently.
 *
 * This is the reboot self-healing leg: when the target claude
 * session dies + comes back (respawn / restart / crash), the ding
 * sidecar's `deliver` fails during the down window and the file is
 * dropped after MAX_DELIVER_RETRIES. Once the session returns,
 * this tick catches the still-unarchived file and re-pokes.
 *
 * 60s per cos's tuning: fast enough that the capstone-eval's ~220s
 * LOOP-CLOSED window sees a re-poke, cheap enough to run forever
 * (readdirSync of a folder is microseconds).
 */
const DEFAULT_RESCAN_INTERVAL_MS = 60_000;

/**
 * Quiet period after a SUCCESSFUL delivery before the re-scan will
 * re-poke the same file. Gives the agent time to read + archive
 * before we nudge again. If the agent is healthy + mid-processing,
 * the archive lands during this window and the file is gone from
 * the inbox before the next re-scan looks at it — no wasted poke.
 * If the agent parks (delivered but never drained — mid-`--resume`
 * that skipped the boot ritual, or a wedged reply), the file stays
 * unarchived and we re-poke after this window elapses.
 *
 * 90s per capstone tuning: the capstone grades LOOP-CLOSED in
 * ~220s, so the quiet window MUST be < 220s for a delivered-but-
 * parked agent to be re-poked in time. 5 min (300s) missed the
 * window and left parked agents stuck; 90s means at most ~150s
 * total (60s scan interval + 90s quiet) before a re-poke — well
 * inside the 220s grade window. Trade-off: a healthy agent
 * mid-read gets a re-nudge if they take longer than 90s to
 * archive — acceptable noise, and archive latency is typically
 * much shorter than that.
 */
const DEFAULT_RESCAN_QUIET_AFTER_DELIVERY_MS = 90_000;

/**
 * brief-036 (typing-aware ding) defaults. The guard peeks the target
 * pane twice `DEFAULT_PEEK_DIFF_MS` apart; if the frames differ (active
 * typing/output) or the input line has un-submitted text, it holds the
 * poke and retries every `DEFAULT_HOLD_RETRY_MS`, up to
 * `DEFAULT_MAX_HOLDS` times. At the cap it force-delivers, but ONLY once
 * the frame is static — a still-changing (mid-turn) frame keeps holding,
 * because a submit queued into an active Claude Code turn seeds a
 * queued-input re-poke bug (deferred, never dropped: urgent bypasses and
 * the re-scan re-pokes an un-archived message once the pane idles).
 * Urgent (`priority: high`) messages skip the guard. ~60s worst-case
 * hold for a pane that goes idle; longer while it stays busy. All
 * env-overridable in `cmdDingCli`.
 */
const DEFAULT_PEEK_DIFF_MS = 300;
const DEFAULT_HOLD_RETRY_MS = 20_000;
const DEFAULT_MAX_HOLDS = 3;
/**
 * Input-area busy heuristic: the pane's last non-blank line matches
 * this when it holds a prompt glyph (`>`/`❯`/`›`/`$`/`#`) followed by
 * at least one non-space char — i.e. text typed but not yet submitted.
 * Best-effort + per-harness-tunable via `ST_DING_INPUT_PATTERN`; an
 * empty prompt (`> `) does not match.
 */
const DEFAULT_INPUT_PATTERN = /[>❯›$#][ \t]*\S/;
/**
 * brief-036 refinement: how many consecutive retries an un-submitted
 * input line must stay UNCHANGED before we treat it as "walked away
 * mid-type" and preserve-and-deliver (rather than holding forever for
 * a human who isn't coming back). Env: `ST_DING_INPUT_STALE_MAX`.
 */
const DEFAULT_INPUT_STALE_MAX = 3;

/**
 * Run the ding daemon. Resolves when the AbortSignal aborts (or
 * when the upstream watcher exits, which only happens on signal in
 * normal operation). Production callers from `cmdDingCli` expect
 * this to run forever; tests pass a tight signal.
 */
export async function runDing(deps: DingDeps): Promise<void> {
  const intervalMs = deps.intervalMs ?? DEFAULT_INTERVAL_MS;
  const rawSend = deps.ptySend ?? defaultPtySend;
  const log = deps.stderr ?? ((s) => process.stderr.write(s));
  const debug = deps.debug === true;
  const dbg = (msg: string): void => {
    if (debug) log(`[st ding debug] ${msg}\n`);
  };

  // Send serialization: every `pty send` invocation goes through this
  // chain so `--with-delay 0.5`-widened windows can't interleave (a
  // second send starting mid-way through the first's text-then-Enter
  // sequence would let text-A/text-B/return-A/return-B garble on the
  // receiving terminal). The chain awaits the previous send's
  // completion (regardless of its outcome) before invoking the next.
  let sendChain: Promise<unknown> = Promise.resolve();
  const send: PtySender = async (sessionName, sequences) => {
    const prev = sendChain;
    const p = (async () => {
      await prev.catch(() => undefined);
      const result = await rawSend(sessionName, sequences);
      if (debug) {
        // Preview line: extract the first non-key sequence for a
        // human-readable hint (usually the [DING] line body).
        const preview =
          sequences.find((s) => !s.startsWith('key:')) ?? sequences[0] ?? '';
        const shortPreview =
          preview.length > 80 ? `${preview.slice(0, 77)}...` : preview;
        const stderrTail = result.stderr.trim().slice(-120);
        dbg(
          `pty send → session="${sessionName}" status=${result.status}` +
            ` preview=${JSON.stringify(shortPreview)}` +
            (stderrTail.length > 0 ? ` stderr=${JSON.stringify(stderrTail)}` : '')
        );
      }
      return result;
    })();
    sendChain = p.catch(() => undefined);
    return p;
  };

  // brief-036: typing-aware pane guard config. Env-overridable in
  // cmdDingCli; deps override for tests.
  const peek = deps.ptyPeek ?? defaultPtyPeek;
  const paste = deps.ptyPaste ?? defaultPtyPaste;
  const paneGuardOn = deps.paneGuard ?? true;
  const peekDiffMs = deps.peekDiffMs ?? DEFAULT_PEEK_DIFF_MS;
  const holdRetryMs = deps.holdRetryMs ?? DEFAULT_HOLD_RETRY_MS;
  const maxHolds = deps.maxHolds ?? DEFAULT_MAX_HOLDS;
  const inputGuardOn = deps.inputGuard ?? true;
  const inputPattern = deps.inputPattern ?? DEFAULT_INPUT_PATTERN;
  const inputStaleMax = deps.inputStaleMax ?? DEFAULT_INPUT_STALE_MAX;

  // Outcome of a guarded delivery attempt. `held` carries the tracking
  // state the caller must persist on the requeued event so the next
  // retry can tell "still typing" from "walked away".
  type GuardOutcome =
    | { kind: 'delivered' }
    | { kind: 'failed' }
    | { kind: 'held'; inputText: string; staleCount: number };

  async function normalDeliver(ev: BufferedEvent): Promise<GuardOutcome> {
    const ok = await deliver(send, deps.ptySession, ev, log);
    return ok ? { kind: 'delivered' } : { kind: 'failed' };
  }

  /**
   * brief-036 refinement: deliver WITHOUT clobbering the human's
   * un-submitted input. Bracketed-paste a leading newline + the ding
   * (appended after their cursor text — nothing submits yet), then
   * submit, so the turn is "<their text>\n[DING] …" and nothing typed
   * is lost. (The exact "insert newline without submit" keystrokes are
   * pending confirmation on a live Claude Code pane — isolated here so
   * that's a one-function change.)
   */
  async function preserveDeliver(ev: BufferedEvent): Promise<GuardOutcome> {
    const dingText = buildDingText(ev);
    let pasteRes: { status: number; stderr: string };
    try {
      pasteRes = await paste(deps.ptySession, `\n${dingText}`);
    } catch (err) {
      log(`st ding: preserve-deliver paste failed: ${errMsg(err)}\n`);
      return { kind: 'failed' };
    }
    if (pasteRes.status !== 0) {
      const tail = pasteRes.stderr.trim().slice(-200);
      log(
        `st ding: preserve-deliver paste to "${deps.ptySession}" exited ${pasteRes.status}${
          tail ? `: ${tail}` : ''
        }\n`
      );
      return { kind: 'failed' };
    }
    // Submit the whole buffer (their text + the pasted ding).
    let submitRes: { status: number; stderr: string };
    try {
      submitRes = await send(deps.ptySession, ['key:return']);
    } catch (err) {
      log(`st ding: preserve-deliver submit failed: ${errMsg(err)}\n`);
      return { kind: 'failed' };
    }
    if (submitRes.status !== 0) {
      log(
        `st ding: preserve-deliver submit to "${deps.ptySession}" exited ${submitRes.status}\n`
      );
      return { kind: 'failed' };
    }
    dbg(`preserve-delivered ${ev.filename} (kept un-submitted input)`);
    return { kind: 'delivered' };
  }

  /**
   * brief-036: decide whether to deliver, hold, or preserve-and-deliver.
   *   - urgent (priority high) or guard off → deliver now, no peek.
   *   - empty input + frame static → deliver (idle / walked away).
   *   - empty input + frame changing → hold (mid-turn). The hold cap
   *     force-delivers ONLY once the frame is static — never into an
   *     active turn, since a submit queued mid-turn seeds Claude Code's
   *     queued-input re-poke bug.
   *   - un-submitted input CHANGING (frame OR text) → active (mid-turn
   *     or typing) → hold (don't interrupt / don't seed the queue).
   *   - un-submitted input + frame BOTH static for `inputStaleMax`
   *     retries (or the hold cap, which is likewise gated on a static
   *     frame) → walked-away-mid-type → preserve-and-deliver.
   * A peek failure → deliver (never block on a peek problem).
   */
  async function guardedDeliver(ev: BufferedEvent): Promise<GuardOutcome> {
    const holds = ev.holds ?? 0;
    const urgent = ev.priority === 'high';
    if (urgent || !paneGuardOn) {
      if (urgent && paneGuardOn) {
        dbg(`urgent (priority=high) → skipping pane guard for ${ev.filename}`);
      }
      return normalDeliver(ev);
    }

    const forceCap = holds >= maxHolds;
    const a = await assessPane(peek, deps.ptySession, { diffMs: peekDiffMs });
    if (!a.ok) return normalDeliver(ev); // peek failed → deliver

    const inputText =
      inputGuardOn && hasInputText(a.inputLine, inputPattern) ? a.inputLine : '';
    const hasInput = inputText !== '';

    // An actively-changing frame means the pane is mid-turn. NEVER
    // submit into it — not even at the hold cap. A submit that lands
    // while Claude Code is processing a turn goes into CC's own
    // queued-input buffer and (a CC-side bug) is re-submitted on every
    // subsequent turn, surfacing as the same [DING] re-poking the agent
    // ~once per turn indefinitely. So the cap force-delivers only once
    // the frame has gone STATIC (idle prompt / genuinely walked away),
    // which is exactly when a submit is safe. This defers — never
    // drops: priority=high bypasses the guard entirely (above), and the
    // periodic re-scan keeps an undelivered message un-archived until an
    // idle moment lands it.

    // No un-submitted text to protect.
    if (!hasInput) {
      if (a.frameChanged) {
        dbg(`frame changing (mid-turn) → holding ${ev.filename} (hold ${holds + 1}; cap won't force into an active turn)`);
        return { kind: 'held', inputText: '', staleCount: 0 };
      }
      return normalDeliver(ev); // frame static (idle / walked away) → safe to submit
    }

    // Un-submitted input present. A changing frame OR input text that
    // changed across retries → the pane is active (mid-turn, or a human
    // is typing) → hold, regardless of the cap (see above). Only once
    // BOTH frame and input are static do we treat it as walked-away-
    // mid-type and preserve-deliver (safe: a static frame is not
    // mid-turn, so the submit won't be queued).
    const changed =
      a.frameChanged ||
      ev.lastInputText === undefined ||
      ev.lastInputText !== inputText;
    if (changed) {
      dbg(`pane active (frame/input changing) → holding ${ev.filename} (hold ${holds + 1})`);
      return { kind: 'held', inputText, staleCount: 1 };
    }
    const staleCount = (ev.inputStaleCount ?? 1) + 1;
    if (forceCap || staleCount >= inputStaleMax) {
      dbg(`input stale ${staleCount}/${inputStaleMax} + frame static → walked away, preserve-deliver ${ev.filename}`);
      return preserveDeliver(ev);
    }
    dbg(`input unchanged ${staleCount}/${inputStaleMax} → holding ${ev.filename}`);
    return { kind: 'held', inputText, staleCount };
  }

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
  // Filenames whose `buildEvent` failed (e.g. peer's atomic write
  // races the watcher fire → read sees mid-rename / partial file).
  // Retried on each flush tick. At-least-once semantics — better than
  // silently dropping a notice on a transient FS race.
  const readPending: Filename[] = [];
  // Last successful delivery timestamp per filename. Used by the
  // periodic re-scan tick to skip files the agent was recently
  // notified about (giving them time to process before we re-poke).
  // Entries are pruned lazily at the top of runRescanTick when the
  // file is no longer in the inbox (archived).
  const deliveredAt = new Map<Filename, number>();
  let timer: ReturnType<typeof setInterval> | undefined;
  // Guard against re-entrant tryFlush: setInterval schedules the
  // callback at fixed times regardless of whether the previous
  // invocation is still awaiting `deliver`. Two concurrent flushes
  // shifting the same buffer risked out-of-order delivery + wasted
  // work; the guard makes flush single-threaded.
  let flushing = false;

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
  let lastTidyFired = { inbox: false };
  async function runTidyTick(): Promise<void> {
    let state: State;
    try {
      state = await deps.st.getStatus(deps.identity);
    } catch (err) {
      log(`st ding: tidy getStatus failed: ${errMsg(err)}\n`);
      return; // best-effort; don't arm dedup on errors
    }
    // Gate: busy/dnd/unknown → no emit, no lastFired update. Drift
    // accumulates; next eligible tick catches up.
    if (TIDY_GATE_STATES.has(state)) return;
    let drift: DriftResult;
    try {
      const driftOpts: { now?: () => number } = {};
      if (deps.tidyNow !== undefined) driftOpts.now = deps.tidyNow;
      drift = evaluateDrift(deps.identity, deps.st.root, driftOpts);
    } catch (err) {
      log(`st ding: tidy evaluate failed: ${errMsg(err)}\n`);
      return;
    }
    const newCondition = drift.inbox && !lastTidyFired.inbox;
    if (newCondition && drift.body.length > 0) {
      const text = formatTidyLine(drift);
      let result: { status: number; stderr: string };
      try {
        result = await send(deps.ptySession, [text, 'key:return']);
      } catch (err) {
        log(`st ding: tidy pty send failed: ${errMsg(err)}\n`);
        // Don't arm lastFired — we want a retry on next tick.
        return;
      }
      if (result.status !== 0) {
        const tail = result.stderr.trim().slice(-200);
        log(
          `st ding: tidy pty send to "${deps.ptySession}" exited ${result.status}${
            tail ? `: ${tail}` : ''
          }\n`
        );
        return; // same — leave lastFired alone for retry
      }
    }
    // Update lastFired on every eligible tick (not just emits) so a
    // drift that clears stops counting as "old news" — only its
    // recurrence-after-clear re-fires.
    lastTidyFired = { inbox: drift.inbox };
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
  // Startup-grace state. The ding sidecar racing pty registration is
  // the load-bearing case: evals-claude's live ding-mode run caught
  // this as the reason `--ding` delivered NOTHING unattended. The
  // ding starts BEFORE the agent's pty session is registered → the
  // first tick sees "target gone" → the daemon exits → being
  // ephemeral, it never comes back. Fix: only trip the exit path
  // AFTER we've seen the target alive at least once. Robust to any
  // launch timing; no timeout needed (an operator who typo'd the
  // session name will notice from other signals — hooks-loud, no
  // delivered `[DING]`s, etc.).
  //
  // Once we've seen alive, revert to normal exit-when-gone
  // behavior — a target that WAS alive but is now gone is a real
  // "session ended" signal, not a race.
  let seenTargetAlive = false;
  // Bookkeeping to keep the "still waiting" log a single line, not
  // a per-tick spam.
  let loggedWaitingForTarget = false;
  // Session-flap debounce: require N consecutive "gone" observations
  // before tripping the exit-when-gone path. A pty `--permanent`
  // session is auto-restarted by pty's supervisor; between the old
  // process exiting and the pidfile being rewritten there's a
  // window where `process.kill(pid, 0)` returns ESRCH but the
  // session is actually about to come back. Without debounce, ding
  // exits right when its target has just briefly flapped — its
  // supervisor restarts ding eventually but arrivals during the gap
  // are missed. Debounce closes that hole.
  let consecutiveMisses = 0;
  function runSessionWatchTick(): void {
    let alive: boolean;
    try {
      alive = isSessionAlive(deps.ptySession);
    } catch (err) {
      // Probe failure: be conservative — treat as alive so we don't
      // tear down on a transient permission glitch. Log so the
      // operator can investigate. Also reset the miss counter — an
      // unknown state shouldn't count as a "gone" observation.
      log(`st ding: session-alive check failed: ${errMsg(err)}\n`);
      consecutiveMisses = 0;
      return;
    }
    if (alive) {
      seenTargetAlive = true;
      consecutiveMisses = 0;
      return;
    }
    // alive === false
    if (!seenTargetAlive) {
      // Startup grace: the target hasn't appeared yet. Log once so
      // the operator sees the daemon is waiting (not dead), then
      // stay silent until the target appears or we get an external
      // signal.
      if (!loggedWaitingForTarget) {
        log(
          `st ding: target session "${deps.ptySession}" not yet ` +
            `registered; waiting for it to appear before enabling the ` +
            `exit-when-gone watch.\n`
        );
        loggedWaitingForTarget = true;
      }
      return;
    }
    // Post-startup, target has flipped from alive to gone. Debounce:
    // require SESSION_GONE_DEBOUNCE_MISSES consecutive misses before
    // aborting. A permanent-session flap (~1-2 misses at the default
    // 30s interval, or ~1 miss at aggressive test intervals) rides
    // through cleanly; a real "session ended" (target won't come
    // back) still trips the exit path within a couple of ticks.
    consecutiveMisses++;
    if (consecutiveMisses < SESSION_GONE_DEBOUNCE_MISSES) {
      log(
        `st ding: target session "${deps.ptySession}" appears gone ` +
          `(miss ${consecutiveMisses}/${SESSION_GONE_DEBOUNCE_MISSES}); ` +
          `debouncing before exit.\n`
      );
      return;
    }
    log(
      `st ding: target session "${deps.ptySession}" is gone; exiting.\n`
    );
    internalAc.abort();
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
    const outcome = refreshIdentityStatus(deps.identity, deps.st.root);
    if (outcome === 'error') {
      log(
        `st ding: status refresh for "${deps.identity}" failed (best-effort, will retry next tick).\n`
      );
    } else if (outcome === 'left-corrupt') {
      log(
        `st ding: status file for "${deps.identity}" contains invalid content; refresh skipped.\n`
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
    if (flushing) return; // re-entry guard — setInterval doesn't skip
    flushing = true;
    try {
      // Retry any pending reads first — a peer's atomic-rename race
      // may have resolved. Failed reads stay in readPending; a
      // successful read pushes into buffer for the drain below.
      if (readPending.length > 0) {
        const attemptList = readPending.splice(0);
        for (const fn of attemptList) {
          try {
            const ev = await buildEvent(deps.st, deps.identity, fn);
            buffer.push(ev);
          } catch (err) {
            log(
              `st ding: read retry still failing for ${fn}: ${errMsg(err)}\n`
            );
            readPending.push(fn);
          }
        }
      }
      if (buffer.length === 0 && readPending.length === 0) {
        disarmTimer();
        return;
      }
      let state: State;
      try {
        state = await deps.st.getStatus(deps.identity);
      } catch (err) {
        log(
          `st ding: getStatus failed: ${errMsg(err)}\n`
        );
        return;
      }
      if (SUPPRESS_STATES.has(state)) {
        // Still busy — keep the timer armed.
        return;
      }
      // Drain in chronological insertion order. On a `deliver` failure,
      // requeue at the HEAD with an incremented retry count so we
      // retry-then-move-on rather than blocking newer arrivals behind
      // a broken target. Capped by MAX_DELIVER_RETRIES. brief-036:
      // events held for a busy pane carry a `notBefore` — not-yet-due
      // ones are deferred (re-checked on a later tick), and a fresh
      // hold is requeued with an incremented `holds` count until the
      // cap forces delivery (never dropped).
      const deferred: BufferedEvent[] = [];
      while (buffer.length > 0) {
        const ev = buffer.shift()!;
        if (ev.notBefore !== undefined && ev.notBefore > msNow()) {
          deferred.push(ev); // still inside its hold window
          continue;
        }
        const outcome = await guardedDeliver(ev);
        if (outcome.kind === 'held') {
          deferred.push({
            ...ev,
            holds: (ev.holds ?? 0) + 1,
            notBefore: msNow() + holdRetryMs,
            lastInputText: outcome.inputText,
            inputStaleCount: outcome.staleCount,
          });
          continue;
        }
        if (outcome.kind === 'failed') {
          const retries = (ev.retries ?? 0) + 1;
          if (retries >= MAX_DELIVER_RETRIES) {
            log(
              `st ding: giving up on ${ev.filename} after ${MAX_DELIVER_RETRIES} deliver attempts; ` +
                `check pty session "${deps.ptySession}" or restart ding — ` +
                `will re-attempt via the periodic backlog re-scan\n`
            );
            continue; // drop after cap — periodic re-scan retries later
          }
          buffer.unshift({ ...ev, retries });
          // Break out of the while: don't spin on the same failing
          // event within one flush call. Timer will re-fire.
          break;
        }
        deliveredAt.set(ev.filename, msNow());
      }
      // Re-queue any held/not-yet-due events; the flush timer stays
      // armed while the buffer is non-empty, so they get re-checked.
      if (deferred.length > 0) buffer.push(...deferred);
      if (buffer.length === 0 && readPending.length === 0) {
        disarmTimer();
      }
    } finally {
      flushing = false;
    }
  }

  // Startup-race dedup: during the first STARTUP_DEDUP_WINDOW_MS the
  // watcher iteration + `scanStartupBacklog` run concurrently, and
  // both feed `onEvent`. This set catches the overlap-window arrivals
  // that would otherwise be double-delivered. Cleared after the
  // window closes; subsequent `onEvent` calls skip the check.
  const startupSeen = new Set<string>();
  let startupPhase = true;

  async function onEvent(
    filename: Filename,
    opts: { bypassStartupDedup?: boolean } = {}
  ): Promise<void> {
    // Startup-window dedup — either the scan or the watcher wins for
    // a given filename, the other is a no-op. The periodic re-scan
    // (post-startup semantics) bypasses this so a file already seen
    // during boot can still be re-poked on later ticks.
    if (startupPhase && opts.bypassStartupDedup !== true) {
      if (startupSeen.has(filename)) return;
      startupSeen.add(filename);
    }
    let state: State;
    try {
      state = await deps.st.getStatus(deps.identity);
    } catch (err) {
      log(`st ding: getStatus failed: ${errMsg(err)}\n`);
      // If we can't read status, lean toward delivering — better
      // than silently dropping a st message.
      state = 'available';
    }
    let event: BufferedEvent;
    try {
      event = await buildEvent(deps.st, deps.identity, filename);
    } catch (err) {
      // Buffer the bare filename for retry on the next flush tick.
      // Peer's atomic write races the watcher fire → read sees
      // mid-rename → transient error → succeeds on retry. At-least-
      // once. Comment at the original ("lean toward delivering —
      // better than silently dropping") applies here too.
      log(`st ding: read failed for ${filename}: ${errMsg(err)}\n`);
      readPending.push(filename);
      ensureTimerArmed();
      return;
    }
    if (SUPPRESS_STATES.has(state)) {
      buffer.push(event);
      ensureTimerArmed();
      return;
    }
    // Direct delivery path — brief-036: gate through the pane guard.
    // On 'held' (busy pane) requeue with an incremented hold count + a
    // notBefore; on 'fail' retry via `retries` (same MAX_DELIVER_RETRIES
    // cap as the flush-loop drain). Either way the flush timer
    // re-attempts.
    const outcome = await guardedDeliver(event);
    if (outcome.kind === 'held') {
      buffer.push({
        ...event,
        holds: (event.holds ?? 0) + 1,
        notBefore: msNow() + holdRetryMs,
        lastInputText: outcome.inputText,
        inputStaleCount: outcome.staleCount,
      });
      ensureTimerArmed();
      return;
    }
    if (outcome.kind === 'failed') {
      const retries = (event.retries ?? 0) + 1;
      if (retries >= MAX_DELIVER_RETRIES) {
        log(
          `st ding: giving up on ${event.filename} after ${MAX_DELIVER_RETRIES} deliver attempts; ` +
            `check pty session "${deps.ptySession}" or restart ding — ` +
            `will re-attempt via the periodic backlog re-scan\n`
        );
        return;
      }
      buffer.push({ ...event, retries });
      ensureTimerArmed();
      return;
    }
    deliveredAt.set(event.filename, msNow());
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
        statusPath(deps.identity, deps.st.root)
      ).mtimeMs;
    } catch {
      // missing or unreadable status file → treat as 0 (all inbox files
      // are eligible). A fresh agent that never set status still gets
      // the backlog replayed.
    }
    const dir = inboxDir(deps.identity, deps.st.root);
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
    if (debug) {
      dbg(
        `startup scan: statusMtimeMs=${statusMtimeMs} inbox=${entries.length} ` +
          `eligible=${eligible.length}` +
          (eligible.length > 0
            ? ` files=[${eligible.map((e) => e.filename).join(',')}]`
            : '')
      );
    }
    for (const { filename } of eligible) {
      await onEvent(asFilename(filename));
    }
  }

  // Reboot self-healing: periodic re-scan tick. Re-pokes files that
  // are still in the inbox (unarchived) and haven't been delivered
  // recently. Covers three failure modes the initial scan +
  // watcher-fire path miss:
  //   1. Ding survived but the target claude session died. `deliver`
  //      failed during the down window and the file was dropped
  //      after MAX_DELIVER_RETRIES. Once claude respawns, the next
  //      tick finds the unarchived file (no `deliveredAt` entry) and
  //      re-pokes.
  //   2. Agent was busy → available flipped. The buffer flush covers
  //      this normally; the re-scan is idempotent (files in the
  //      buffer are skipped).
  //   3. Agent respawned via `--resume` and skipped the boot ritual.
  //      Backlog sits unarchived; after RESCAN_QUIET_AFTER_DELIVERY_MS
  //      elapses (~5 min), we re-nudge.
  //
  // Buffer + readPending are the "in-flight" markers — files already
  // being retried. Skip them so the re-scan doesn't double-buffer.
  const rescanIntervalMs =
    deps.rescanIntervalMs ?? DEFAULT_RESCAN_INTERVAL_MS;
  const rescanQuietAfterDeliveryMs =
    deps.rescanQuietAfterDeliveryMs ??
    DEFAULT_RESCAN_QUIET_AFTER_DELIVERY_MS;
  let rescanTimer: ReturnType<typeof setInterval> | undefined;
  async function runRescanTick(): Promise<void> {
    const dir = inboxDir(deps.identity, deps.st.root);
    let entries: string[];
    try {
      entries = readdirSync(dir);
    } catch {
      return; // inbox missing / unreadable — nothing to scan
    }
    const inboxSet = new Set<string>();
    for (const name of entries) {
      if (validFilename(name)) inboxSet.add(name);
    }
    // Prune deliveredAt: forget files that were archived (no longer
    // in inbox) so the map doesn't grow unbounded. Rare-cost O(map)
    // pass; the map's size tracks unarchived files, typically small.
    for (const fn of deliveredAt.keys()) {
      if (!inboxSet.has(fn)) deliveredAt.delete(fn);
    }
    const now = msNow();
    // Chronological by filename so re-pokes stay in original order.
    const sorted = [...inboxSet].sort((a, b) => a.localeCompare(b));
    let inFlightSkipped = 0;
    let quietSkipped = 0;
    let attempted = 0;
    const attemptedFiles: string[] = [];
    for (const name of sorted) {
      const fn = asFilename(name);
      // Skip in-flight: buffered for later flush, or read-retry pending.
      if (buffer.some((e) => e.filename === fn)) {
        inFlightSkipped++;
        continue;
      }
      if (readPending.includes(fn)) {
        inFlightSkipped++;
        continue;
      }
      // Skip recently-delivered: give the agent time to process
      // before we re-nudge for the same file.
      const last = deliveredAt.get(fn);
      if (last !== undefined && now - last < rescanQuietAfterDeliveryMs) {
        quietSkipped++;
        continue;
      }
      attempted++;
      attemptedFiles.push(name);
      // Fresh candidate → re-poke via onEvent (respects busy/dnd,
      // buffers if suppressed, retries on transient read failure).
      // Bypass startup dedup: this file may have been startup-seen,
      // but the re-scan is a first-class re-poke trigger, not a
      // startup-race dedup event.
      await onEvent(fn, { bypassStartupDedup: true });
    }
    if (debug) {
      dbg(
        `rescan tick: inbox=${inboxSet.size}` +
          ` in-flight-skipped=${inFlightSkipped}` +
          ` quiet-skipped=${quietSkipped}` +
          ` attempted=${attempted}` +
          (attempted > 0
            ? ` files=[${attemptedFiles.join(',')}]`
            : '')
      );
    }
  }
  function startRescanTick(): void {
    if (rescanIntervalMs <= 0) return;
    rescanTimer = setInterval(() => {
      void runRescanTick();
    }, rescanIntervalMs);
    rescanTimer.unref?.();
  }
  function stopRescanTick(): void {
    if (rescanTimer !== undefined) {
      clearInterval(rescanTimer);
      rescanTimer = undefined;
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

  // Reboot self-healing: arm the periodic backlog re-scan.
  startRescanTick();

  // brief-035 t2 (revised): arm the watcher BEFORE the startup scan
  // to close the race window a scan-then-watch ordering left open.
  // Historic ordering: scan → watch, with the scan reading a readdir
  // snapshot and the watcher armed after. Files arriving BETWEEN the
  // scan's readdir and the watcher's arm were in neither set — a
  // silent-drop race on daemon startup. Fix: run the watcher's
  // for-await loop in a concurrent async task, THEN run the scan.
  // Both feed `onEvent`, which dedups via `startupSeen` for the
  // first STARTUP_DEDUP_WINDOW_MS. After the window closes, dedup
  // stops (drops the set to free memory) — the watcher is the sole
  // event source from that point on.
  const startupDedupTimer = setTimeout(() => {
    startupPhase = false;
    startupSeen.clear();
  }, STARTUP_DEDUP_WINDOW_MS);
  startupDedupTimer.unref?.();

  const watcherPromise = (async () => {
    const watchOpts: Parameters<St['watch']>[1] = {
      withSubject: true,
      sinceNow: true,
    };
    watchOpts.signal = signal;
    try {
      for await (const ev of deps.st.watch(
        deps.identity,
        watchOpts
      ) as AsyncIterable<WatchEvent>) {
        if (ev.folder !== 'inbox') continue;
        await onEvent(ev.filename);
      }
    } catch (err) {
      // AbortError is expected when the signal fires; surface others.
      if (
        !(err instanceof Error && err.name === 'AbortError') &&
        !signal.aborted
      ) {
        log(`st ding: watcher errored: ${errMsg(err)}\n`);
      }
    }
  })();

  // Now run the startup scan concurrently. Any file that arrives
  // during the scan is caught by the (already-armed) watcher; both
  // sources dedup via `startupSeen`. Chronological ordering within
  // the scan is preserved by its own sort; concurrent watcher events
  // slot in via the buffer as they arrive.
  try {
    await scanStartupBacklog();
  } catch (err) {
    log(`st ding: startup scan errored: ${errMsg(err)}\n`);
  }

  // Watcher owns the daemon lifetime — it exits when the signal
  // aborts. Await here so `runDing` doesn't resolve until the loop
  // has actually torn down.
  try {
    await watcherPromise;
  } finally {
    clearTimeout(startupDedupTimer);
    disarmTimer();
    stopTidyTick();
    stopSessionWatch();
    stopStatusRefresh();
    stopRescanTick();
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
 *
 * Prefixed with `[DING] ` (same rationale as the inbox-arrival
 * notice above) so a scanning agent recognizes it as bus traffic
 * rather than its own output or a human REPL keystroke.
 */
function formatTidyLine(drift: DriftResult): string {
  return `[DING] tidy-check: inbox=${drift.detail.inboxStaleCount} (oldest ${formatAge(drift.detail.oldestInboxAgeMs)}).`;
}

function formatAge(ms: number): string {
  if (ms < 60_000) return `${Math.round(ms / 1000)}s`;
  if (ms < 60 * 60_000) return `${Math.round(ms / 60_000)}m`;
  if (ms < 24 * 60 * 60_000) return `${Math.round(ms / (60 * 60_000))}h`;
  return `${Math.round(ms / (24 * 60 * 60_000))}d`;
}

async function buildEvent(
  st: St,
  identity: Identity,
  filename: Filename
): Promise<BufferedEvent> {
  // Read the file to extract `from` for the notice. Errors propagate
  // to the caller (which logs + drops the event).
  const r = await st.read(identity, filename);
  return {
    filename,
    from: r.message.from,
    ...(r.message.subject !== undefined && { subject: r.message.subject }),
    ...(r.message.priority !== undefined && { priority: r.message.priority }),
  };
}

/**
 * Ding-delivered notification text. The `[DING] ` prefix marks the
 * line as bus traffic so it's visually distinct from three other
 * things an agent might see in its terminal:
 *   - MCP `<channel source="…">` injection blocks (MCP path only;
 *     ding-mode agents don't get these — their launcher's
 *     bus-instructions file describes the ding poke flow instead).
 *   - The agent's own output printed to its REPL.
 *   - A human typing directly at the REPL.
 *
 * The prefix also lets a ding-mode agent's bus-instructions file
 * reference an unambiguous string pattern for the poke-handling
 * flow ("when you see [DING] X, do Y").
 *
 * Post-rename naming: `smalltalk message`, not `st message` —
 * this is user-visible bus traffic and matches the CLI the agent
 * uses to act on it (`st message …`).
 */
function buildDingText(ev: BufferedEvent): string {
  const subject = ev.subject ?? '(no subject)';
  const from = ev.from === '' ? 'unknown' : ev.from;
  return `[DING] new smalltalk message: ${subject} (from ${from}); check your inbox`;
}

function buildSequences(ev: BufferedEvent): string[] {
  return [buildDingText(ev), 'key:return'];
}

async function deliver(
  send: PtySender,
  sessionName: string,
  ev: BufferedEvent,
  log: (s: string) => void
): Promise<boolean> {
  // Returns true on success, false on failure. Callers requeue on
  // false so a transient pty error (target respawning, ECHILD/EPIPE
  // on the send subprocess pipe, brief supervisor hiccup) doesn't
  // silently drop a notice. The at-least-once delivery rule.
  const sequences = buildSequences(ev);
  let result: { status: number; stderr: string };
  try {
    result = await send(sessionName, sequences);
  } catch (err) {
    log(`st ding: pty send failed: ${errMsg(err)}\n`);
    return false;
  }
  if (result.status !== 0) {
    const tail = result.stderr.trim().slice(-200);
    log(
      `st ding: pty send to "${sessionName}" exited ${result.status}${
        tail ? `: ${tail}` : ''
      }\n`
    );
    return false;
  }
  return true;
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

// ─── brief-036: typing-aware pane guard ─────────────────────────────────

/** Sleep helper for the frame-diff gap. */
function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * Read the target pane as plain text via the injected peeker. Returns
 * the screen string, or null if the peek failed (non-zero exit or a
 * throw) — callers treat a failed peek as "not busy" so a peek problem
 * never blocks (or drops) a delivery.
 */
async function peekPlain(
  peek: PtyPeeker,
  sessionName: string
): Promise<string | null> {
  try {
    const r = await peek(sessionName);
    if (r.status !== 0) return null;
    return r.stdout;
  } catch {
    return null;
  }
}

/**
 * The pane's last non-blank line (trailing whitespace trimmed), or ''
 * when the screen is entirely blank. This is where a TUI renders its
 * input/prompt line — the text we track to tell active typing from a
 * walked-away-mid-type pane.
 */
function lastNonBlankLine(screen: string): string {
  const lines = screen.split('\n');
  for (let i = lines.length - 1; i >= 0; i--) {
    const line = lines[i]!.replace(/\s+$/, '');
    if (line !== '') return line;
  }
  return '';
}

/**
 * True when `line` looks like a prompt holding un-submitted text (see
 * {@link DEFAULT_INPUT_PATTERN}) — an empty prompt (`> `) does not
 * match.
 */
function hasInputText(line: string, pattern: RegExp): boolean {
  return pattern.test(line);
}

/**
 * brief-036: sample the pane. Peeks twice `diffMs` apart and reports
 * whether the whole frame changed (active output/typing) plus the
 * current input line (last non-blank line). A failed peek → `ok:false`
 * so callers lean toward delivering (never block on a peek problem).
 */
async function assessPane(
  peek: PtyPeeker,
  sessionName: string,
  opts: { diffMs: number }
): Promise<{ ok: boolean; frameChanged: boolean; inputLine: string }> {
  const first = await peekPlain(peek, sessionName);
  if (first === null) return { ok: false, frameChanged: false, inputLine: '' };
  await sleep(opts.diffMs);
  const second = await peekPlain(peek, sessionName);
  if (second === null) {
    return { ok: false, frameChanged: false, inputLine: '' };
  }
  return {
    ok: true,
    frameChanged: first !== second,
    inputLine: lastNonBlankLine(second),
  };
}

const defaultPtyPeek: PtyPeeker = (sessionName) =>
  new Promise((resolve) => {
    let proc: ReturnType<typeof spawn>;
    try {
      proc = spawn('pty', ['peek', '--plain', sessionName], {
        stdio: ['ignore', 'pipe', 'pipe'],
      });
    } catch (err) {
      resolve({ status: -1, stdout: '', stderr: errMsg(err) });
      return;
    }
    let stdout = '';
    let stderr = '';
    proc.stdout?.on('data', (chunk: Buffer) => {
      stdout += chunk.toString('utf8');
    });
    proc.stderr?.on('data', (chunk: Buffer) => {
      stderr += chunk.toString('utf8');
    });
    proc.once('error', (err) => {
      resolve({ status: -1, stdout, stderr: err.message });
    });
    proc.once('close', (status) => {
      resolve({ status: status ?? -1, stdout, stderr });
    });
  });

const defaultPtyPaste: PtyPaster = (sessionName, text) =>
  new Promise((resolve) => {
    let proc: ReturnType<typeof spawn>;
    try {
      proc = spawn('pty', ['send', sessionName, '--paste', text], {
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

// ─── CLI wrapper ────────────────────────────────────────────────────────

export interface PtyProbeResult {
  available: boolean;
  reason?: string;
}

/**
 * PATH robustness: probe for the `pty` binary at daemon boot. Runs
 * `pty --version` synchronously. On success → available. On ENOENT
 * / non-zero exit → `{ available: false, reason: <human-readable> }`.
 * A ding daemon that can't spawn `pty` runs forever with zero
 * successful deliveries — better to exit-with-error at start than
 * silently fail forever. Exported for tests.
 */
export function probePtyOnPath(): PtyProbeResult {
  // `pty --help` returns 0 without touching live session state (unlike
  // `pty ls` which reads the session dir). Safer for a boot-time
  // liveness check; if `pty` is on PATH and executable, this succeeds.
  let probe: ReturnType<typeof spawnSync>;
  try {
    probe = spawnSync('pty', ['--help'], {
      stdio: 'pipe',
      encoding: 'utf8',
    });
  } catch (err) {
    return { available: false, reason: errMsg(err) };
  }
  if (probe.error !== undefined) {
    return { available: false, reason: probe.error.message };
  }
  if (probe.status !== 0) {
    const tail =
      typeof probe.stderr === 'string' && probe.stderr.length > 0
        ? `: ${probe.stderr.trim().slice(0, 200)}`
        : '';
    return {
      available: false,
      reason: `pty --help exited ${probe.status}${tail}`,
    };
  }
  return { available: true };
}

export async function cmdDingCli(
  args: readonly string[],
  ctx: CliContext,
  deps: { ptyProbe?: () => PtyProbeResult } = {}
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
          `usage: ${invokedName(ctx.env)} ding <pty-session> [--identity ID] [--interval MS]\n` +
            '                          [--tidy-interval-ms MS]\n' +
            '                          [--status-refresh-interval-ms MS]\n' +
            '                          [--no-exit-when-session-gone]\n\n' +
            '  Watches <identity>/inbox/ and pty-sends a notice into\n' +
            '  <pty-session> on every new arrival. Buffers while status\n' +
            '  is busy/dnd; flushes when status flips back to available.\n' +
            '  Also runs a periodic tidy-check that pty-sends a drift\n' +
            '  summary when inbox is out of date, AND\n' +
            "  refreshes the watched identity's status file mtime so\n" +
            "  the identity doesn't fall into `unknown` over long\n" +
            "  inactivity (mirrors the MCP server's brief-023 behavior\n" +
            "  for Codex agents that don't run an MCP server per identity).\n" +
            '  Exits cleanly when the target pty session is gone.\n' +
            '  Long-running — pair with `pty up` for supervision.\n\n' +
            '  Examples:\n' +
            `    ${invokedName(ctx.env)} ding my-claude-session --identity cos\n` +
            `    ST_DING_PANE_GUARD=0 ${invokedName(ctx.env)} ding my-session   # opt out of typing-aware guard\n\n` +
            '  --identity ID                    Smalltalk identity to watch. Defaults to $ST_AGENT.\n' +
            '  --interval MS                    Status poll interval while buffered. Default 1000ms.\n' +
            '  --tidy-interval-ms MS            Tidy-check tick interval. Default 20 min.\n' +
            '                                   Set to 0 to disable tidy-check entirely\n' +
            '                                   (push-only mode, pre-brief-031 behavior).\n' +
            '  --status-refresh-interval-ms MS  Status mtime refresh interval. Default 5 min.\n' +
            '                                   Set to 0 to disable.\n' +
            '  --exit-when-session-gone         Exit when the target pty session is gone (default).\n' +
            '  --no-exit-when-session-gone      Keep running even when the target session\n' +
            '                                   is gone (rare; opt-out).\n' +
            '\n' +
            '  Env overrides for the periodic backlog re-scan:\n' +
            '    ST_DING_RESCAN_INTERVAL_MS     Scan interval. Default 60000 (60s).\n' +
            '                                   Set to 0 to disable the re-scan tick.\n' +
            '    ST_DING_RESCAN_QUIET_MS        Quiet window after a successful delivery\n' +
            '                                   before the same file is re-poked. Default\n' +
            '                                   90000 (90s). Tune down for aggressive\n' +
            '                                   re-poking of parked agents.\n' +
            '    ST_DING_DEBUG=1                Verbose diagnostic mode. Emits [st ding\n' +
            '                                   debug] lines to stderr: startup scan\n' +
            '                                   summary, per-rescan-tick summary\n' +
            '                                   (inbox / in-flight-skipped / quiet-\n' +
            '                                   skipped / attempted), and every pty send\n' +
            '                                   attempt (session, status, stderr tail).\n' +
            '                                   Off by default.\n' +
            '\n' +
            '  Env overrides for the typing-aware pane guard (ON by default —\n' +
            '  to turn it OFF for this agent, set ST_DING_PANE_GUARD=0). It peeks\n' +
            '  the pane via `pty peek --plain` and holds a poke while the human\n' +
            '  is actively typing:\n' +
            '    ST_DING_PANE_GUARD=0           Disable the guard entirely (deliver on\n' +
            '                                   arrival, the pre-guard behavior).\n' +
            '    ST_DING_PEEK_DIFF_MS           Gap between the two peek frames used to\n' +
            '                                   detect activity. Default 300 (ms).\n' +
            '    ST_DING_HOLD_RETRY_MS          How long to hold a busy pane before\n' +
            '                                   re-checking. Default 20000 (20s).\n' +
            '    ST_DING_MAX_HOLDS              Max holds before force-delivering anyway\n' +
            '                                   (never dropped). Default 3 (~60s worst\n' +
            '                                   case). Urgent (priority: high) messages\n' +
            '                                   skip the guard entirely.\n' +
            '    ST_DING_INPUT_GUARD=0          Disable the input-line check. Default on.\n' +
            '                                   With it on: un-submitted input that keeps\n' +
            '                                   CHANGING across retries = actively typing\n' +
            '                                   (hold); input that stays UNCHANGED =\n' +
            '                                   walked-away → deliver WITHOUT clobbering\n' +
            '                                   it (newline + ding appended, then submit).\n' +
            '    ST_DING_INPUT_PATTERN          Regex (source) matched against the last\n' +
            '                                   non-blank line to detect un-submitted\n' +
            '                                   input. Default a prompt glyph + text.\n' +
            '    ST_DING_INPUT_STALE_MAX        Consecutive unchanged-input retries before\n' +
            '                                   a stale input line is treated as\n' +
            '                                   walked-away. Default 3.\n'
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
    throw new Error('st ding requires a <pty-session> name');
  }

  const root = ctx.stRoot;
  if (!root) {
    throw new Error('ST_ROOT must be set for `st ding`');
  }
  const identityValue = identityArg ?? ctx.env.ST_AGENT;
  if (!identityValue) {
    throw new Error(
      '`st ding` needs --identity ID or $ST_AGENT to know which inbox to watch'
    );
  }

  // Lazy-import the embeddable factory + asIdentity so non-ding
  // invocations don't pull lib.ts into the dispatcher hot path.
  const { createSt } = await import('../lib.ts');
  const { asIdentity } = await import('../types.ts');
  const { ensureIdentityDirs } = await import('../common.ts');

  const identity = asIdentity(identityValue);

  // Startup-race hardening: ensure the watched identity's folder
  // exists before we hand off to runDing. Otherwise the watcher
  // throws `agent folder missing for <id>` at first poll, the
  // watcher async iterator errors out, `runDing` cleans up its
  // timers, and the whole daemon exits — leaving the target
  // session un-poked forever.
  //
  // The race: convoy (or any supervisor) can spawn the ding
  // sidecar BEFORE the target agent's folder exists (the agent
  // itself hasn't sent its first message yet). Rather than
  // depend on ordering, `st ding` self-heals: create the folder
  // if missing. Idempotent — matches the same lazy-create
  // semantic every other verb uses when the invoker is the
  // identity's own owner.
  ensureIdentityDirs(identity, root);

  const st = createSt({ root, identity });

  // PATH robustness: probe for `pty` on the daemon's PATH BEFORE
  // starting the watcher / timers. A ding daemon that can't spawn
  // `pty` runs forever with zero successful deliveries; fail fast
  // + LOUD is the right posture. Especially load-bearing when ding
  // is launched from a supervisor (launchd/systemd/cron) whose
  // environment strips PATH — the ding sidecar comes up "healthy"
  // but silently drops every notice.
  const probe = deps.ptyProbe ?? probePtyOnPath;
  const probeResult = probe();
  if (!probeResult.available) {
    ctx.stderr(
      `\n` +
        `[st ding] ────────────────────────────────────────\n` +
        `[st ding] The 'pty' binary is NOT available on PATH.\n` +
        `[st ding] st ding cannot deliver any [DING] notices\n` +
        `[st ding] without pty — it's the transport layer.\n` +
        `[st ding]\n` +
        `[st ding] Probe: ${probeResult.reason ?? 'unknown failure'}\n` +
        `[st ding]\n` +
        `[st ding] Fix: install pty and ensure it's on the\n` +
        `[st ding] daemon's PATH. If ding is launched by a\n` +
        `[st ding] supervisor (launchd/systemd), the supervisor\n` +
        `[st ding] typically strips PATH; set PATH explicitly\n` +
        `[st ding] in the unit/plist file. Refusing to start\n` +
        `[st ding] rather than run forever with zero deliveries.\n` +
        `[st ding] ────────────────────────────────────────\n\n`
    );
    return 2;
  }

  const ac = new AbortController();
  const onSig = (): void => ac.abort();
  process.once('SIGINT', onSig);
  process.once('SIGTERM', onSig);

  // Env-overridable knobs for the periodic backlog re-scan. Lets
  // evals + operators tune without a code change or CLI flag. Both
  // are non-negative integers (ms); malformed values are ignored
  // (log to stderr, fall back to defaults) so a typo in an env file
  // can't crash the daemon.
  const rescanIntervalMs = parseEnvMs(
    ctx.env.ST_DING_RESCAN_INTERVAL_MS,
    'ST_DING_RESCAN_INTERVAL_MS',
    ctx.stderr
  );
  const rescanQuietAfterDeliveryMs = parseEnvMs(
    ctx.env.ST_DING_RESCAN_QUIET_MS,
    'ST_DING_RESCAN_QUIET_MS',
    ctx.stderr
  );

  // Diagnostic mode: verbose delivery + rescan-tick trace to stderr.
  // Off by default (production shouldn't pay the log volume). Toggle
  // via ST_DING_DEBUG=1 in the env; any non-'1' value is off.
  const debugMode = ctx.env.ST_DING_DEBUG === '1';

  // brief-036: env-overridable knobs for the typing-aware pane guard.
  // Malformed numeric/regex values are ignored (warn + default) so an
  // env-file typo can't crash the daemon.
  const paneGuard = parseEnvBool(ctx.env.ST_DING_PANE_GUARD);
  const inputGuard = parseEnvBool(ctx.env.ST_DING_INPUT_GUARD);
  const peekDiffMs = parseEnvMs(
    ctx.env.ST_DING_PEEK_DIFF_MS,
    'ST_DING_PEEK_DIFF_MS',
    ctx.stderr
  );
  const holdRetryMs = parseEnvMs(
    ctx.env.ST_DING_HOLD_RETRY_MS,
    'ST_DING_HOLD_RETRY_MS',
    ctx.stderr
  );
  const maxHolds = parseEnvMs(
    ctx.env.ST_DING_MAX_HOLDS,
    'ST_DING_MAX_HOLDS',
    ctx.stderr
  );
  const inputPattern = parseEnvRegExp(
    ctx.env.ST_DING_INPUT_PATTERN,
    'ST_DING_INPUT_PATTERN',
    ctx.stderr
  );
  const inputStaleMax = parseEnvMs(
    ctx.env.ST_DING_INPUT_STALE_MAX,
    'ST_DING_INPUT_STALE_MAX',
    ctx.stderr
  );

  try {
    await runDing({
      st: st,
      identity,
      ptySession,
      ...(intervalMs !== undefined && { intervalMs }),
      ...(tidyIntervalMs !== undefined && { tidyIntervalMs }),
      ...(statusRefreshIntervalMs !== undefined && {
        statusRefreshIntervalMs,
      }),
      ...(rescanIntervalMs !== undefined && { rescanIntervalMs }),
      ...(rescanQuietAfterDeliveryMs !== undefined && {
        rescanQuietAfterDeliveryMs,
      }),
      ...(paneGuard !== undefined && { paneGuard }),
      ...(inputGuard !== undefined && { inputGuard }),
      ...(peekDiffMs !== undefined && { peekDiffMs }),
      ...(holdRetryMs !== undefined && { holdRetryMs }),
      ...(maxHolds !== undefined && { maxHolds }),
      ...(inputPattern !== undefined && { inputPattern }),
      ...(inputStaleMax !== undefined && { inputStaleMax }),
      exitWhenSessionGone,
      debug: debugMode,
      signal: ac.signal,
      stderr: ctx.stderr,
    });
  } finally {
    process.removeListener('SIGINT', onSig);
    process.removeListener('SIGTERM', onSig);
  }
  return 0;
}

/**
 * Parse a non-negative integer (ms) from a env var value. Returns
 * undefined for missing/empty (caller uses the default) and for
 * malformed values (caller uses the default; also emits a stderr
 * warning so an operator misconfiguration is visible).
 */
function parseEnvMs(
  raw: string | undefined,
  name: string,
  stderr: (s: string) => void
): number | undefined {
  if (raw === undefined || raw === '') return undefined;
  if (!/^[0-9]+$/.test(raw)) {
    stderr(
      `st ding: ignoring ${name}=${JSON.stringify(raw)} — must be a non-negative integer (ms); using default\n`
    );
    return undefined;
  }
  return Number(raw);
}

/**
 * brief-036: parse a boolean env override. Returns undefined for
 * missing/empty/unrecognized (caller uses the default). Accepts
 * 1/true/on/yes and 0/false/off/no (case-insensitive).
 */
function parseEnvBool(raw: string | undefined): boolean | undefined {
  if (raw === undefined || raw === '') return undefined;
  const v = raw.trim().toLowerCase();
  if (v === '0' || v === 'false' || v === 'off' || v === 'no') return false;
  if (v === '1' || v === 'true' || v === 'on' || v === 'yes') return true;
  return undefined;
}

/**
 * brief-036: compile a RegExp from an env var (its source). Returns
 * undefined for missing/empty (caller uses the default) and for an
 * invalid pattern (warn + default) so a bad regex can't crash the
 * daemon.
 */
function parseEnvRegExp(
  raw: string | undefined,
  name: string,
  stderr: (s: string) => void
): RegExp | undefined {
  if (raw === undefined || raw === '') return undefined;
  try {
    return new RegExp(raw);
  } catch (err) {
    stderr(
      `st ding: ignoring ${name}=${JSON.stringify(raw)} — invalid regex (${errMsg(err)}); using default\n`
    );
    return undefined;
  }
}
