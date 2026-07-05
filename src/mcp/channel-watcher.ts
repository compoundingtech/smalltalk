// mcp/channel-watcher.ts — chokidar watcher that emits
// `notifications/claude/channel` for every new inbox arrival.
//
// Lazy-imported by `createMcpServer` only when channel mode is on, so
// non-channel `coord mcp` invocations never load chokidar.

import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';

import type { Identity } from '../types.ts';

export interface ChannelWatcherHandle {
  /** Tear the watcher down. Idempotent. */
  close(): Promise<void>;
}

/** Default backstop poll interval when the caller doesn't override. 15s
 *  is a compromise: slow enough that the CPU cost of a `readdirSync`
 *  scan is trivial (even on a big inbox), fast enough that a missed
 *  chokidar event doesn't leave the agent wedged for more than one
 *  channel-response-time from a user's perspective. */
export const DEFAULT_POLL_BACKSTOP_INTERVAL_MS = 15_000;

export interface StartChannelWatcherOpts {
  mcp: McpServer;
  root: string;
  identity: Identity;
  /**
   * Test-only escape hatch. When true, chokidar polls the inbox dir
   * at `pollInterval` ms instead of using FSEvents/inotify. Production
   * leaves this off — FSEvents is dramatically lower-latency. Tests
   * set it because vitest's parallel pool causes FSEvents contention
   * across many short-lived watchers.
   */
  usePolling?: boolean;
  /** Companion to {@link usePolling}. Default 50ms. */
  pollInterval?: number;
  /**
   * brief-020 (HB-4): how often (ms) to run the polling backstop that
   * catches inbox files chokidar's FSEvents-backed watcher may have
   * missed. Defaults to {@link DEFAULT_POLL_BACKSTOP_INTERVAL_MS}. Set
   * to 0 to disable the backstop entirely (not recommended in
   * production; agents will then depend solely on chokidar's stream,
   * which on macOS can silently stop delivering events after long
   * idle periods).
   *
   * The backstop runs `readdirSync(inboxDir)` on each tick, filters to
   * valid LAYOUT filenames, and enqueues any not yet in the `seen`
   * set (which is populated by both chokidar's `add` handler AND the
   * backstop itself, so the two paths dedupe against each other).
   */
  pollBackstopIntervalMs?: number;
  /**
   * brief-020 test seam: when false, don't start chokidar at all — only
   * the polling backstop runs. Used to verify the backstop delivers
   * notifications in isolation, i.e. that the wake path stays honest
   * even if FSEvents silently stops emitting. Production leaves this
   * undefined (chokidar on).
   */
  chokidarEnabled?: boolean;
  /**
   * brief-020: when true, write one-line stderr entries for each
   * chokidar `add`, each poll-backstop discovery, and each notification
   * send. Wired through from `COORD_CHANNEL_DEBUG=1` in the MCP
   * command wrapper. Kept opt-in so a healthy running agent's stderr
   * stays quiet.
   */
  debug?: boolean;
}

/**
 * Start watching `${root}/${identity}/inbox/` for new `.md` files. On
 * each `add` event for a valid LAYOUT-grammar filename, run the
 * pre-emit sweep (so a byte-identical archive zombie doesn't fire),
 * parse the frontmatter, and emit `notifications/claude/channel` with
 * the message body and a small metadata envelope.
 *
 * Returns once the underlying chokidar watcher reports `ready`, so the
 * caller can drop a file into the inbox immediately afterward.
 */
export async function startChannelWatcher(
  opts: StartChannelWatcherOpts
): Promise<ChannelWatcherHandle> {
  const chokidar = await import('chokidar');
  const { existsSync, readdirSync, readFileSync } = await import('node:fs');
  const { join } = await import('node:path');
  const { parseFrontmatter, validFilename, validOutsideFilename } = await import(
    '../common.ts'
  );

  /**
   * A `.md` file that should be delivered — canonical LAYOUT-004
   * OR safe outside grammar. Outside files fire with `from: 'outside'`
   * so the recipient knows the sender isn't verified.
   */
  const isDeliverable = (name: string): boolean =>
    validFilename(name) || validOutsideFilename(name);

  const inboxDir = join(opts.root, opts.identity, 'inbox');
  const chokidarEnabled = opts.chokidarEnabled !== false;
  const pollBackstopMs =
    opts.pollBackstopIntervalMs ?? DEFAULT_POLL_BACKSTOP_INTERVAL_MS;
  const debug = opts.debug === true;

  function debugLog(msg: string): void {
    if (!debug) return;
    // Emit to stderr so it never contaminates the JSON-RPC stdout
    // channel. Prefix with a stable tag so an operator can grep it.
    process.stderr.write(
      `[coord-channel] ${new Date().toISOString()} ${msg}\n`
    );
  }

  // brief-020: dedup set shared between chokidar and the polling
  // backstop. Each path checks-and-adds before enqueueing so a file
  // observed by both paths only emits one channel notification.
  //
  // At-least-once semantic (P5R-F2 fix): the initial `readdirSync`
  // used to seed `seen` with every existing filename — the intent
  // was to avoid replaying "historical files" on restart, with the
  // boot ritual's `coord_msg_ls` handling backlog. Live repro proved
  // that policy too aggressive: a mid-session MCP disconnect +
  // reconnect (Claude Code respawns the stdio process) creates a
  // fresh process whose seed suppressed files that arrived DURING
  // the down-window — the polling backstop can't rescue them
  // because it dedups against the same `seen`, and asyncRewake
  // doesn't force a turn when the agent's actively working.
  //
  // Now we ENQUEUE the initial files instead of seeding them. Each
  // fires exactly once per process, in chronological
  // (<unix-ms>-sorted) order via drain(). Duplicates ACROSS a
  // process restart are accepted per user's spec — "duplicates
  // don't matter to me, we want at-least-once, not at-most-once" —
  // the agent re-reads + archives are harmless (lazy-read sweep
  // clears the byte-identical inbox twin on the next read).
  const seen = new Set<string>();
  // Track the initial-scan filenames so we can enqueue them AFTER
  // the emit pipeline definitions below without another readdir.
  // Populated in the same try/catch that would have seeded `seen`.
  const initialFilenames: string[] = [];
  try {
    for (const name of readdirSync(inboxDir)) {
      if (isDeliverable(name)) {
        seen.add(name);
        initialFilenames.push(name);
      }
    }
    debugLog(
      `initial scan queued ${initialFilenames.length} existing file(s) in inbox=${inboxDir}`
    );
  } catch {
    // inbox dir may not exist yet on very fresh identities; chokidar
    // and the poll backstop will surface files once it does.
  }

  const watcher = chokidarEnabled
    ? chokidar.watch(inboxDir, {
        ignoreInitial: true,
        persistent: true,
        // No awaitWriteFinish: every coord producer (coord.send,
        // tmp+atomic-rename; rsync, atomic-on-completion) writes a
        // single final file. Polling for write stability just adds
        // latency that makes the watcher flake on heavily-loaded test
        // runs.
        ...(opts.usePolling === true && {
          usePolling: true,
          interval: opts.pollInterval ?? 50,
          binaryInterval: opts.pollInterval ?? 50,
        }),
      })
    : null;

  // Burst-aware emit pipeline. chokidar's 'add' callback fires in
  // whatever order the filesystem reports (rsync source-listing order
  // on macOS, generally NOT chronological). For a burst of N files
  // we want notifications in chronological filename order — the
  // <unix-ms>-<rand6>.md grammar already gives us a sort key. We also
  // want one in-flight notification at a time, so two `add` events
  // arriving back-to-back can't interleave their notification awaits.
  //
  // Implementation: filenames land in `pending` first; the drainer
  // sorts the snapshot, emits in order, then loops if more files
  // arrived during the previous batch. Files added while a drain is
  // in flight participate in the next sorted batch.
  const pending: string[] = [];
  let draining = false;

  function enqueue(filepath: string): void {
    pending.push(filepath);
    if (!draining) {
      draining = true;
      void drain().finally(() => {
        draining = false;
      });
    }
  }

  async function drain(): Promise<void> {
    while (pending.length > 0) {
      // Snapshot + sort by basename so a single burst is delivered
      // in chronological order. <unix-ms> prefix dominates; rand6
      // suffix breaks same-millisecond ties deterministically.
      const batch = pending
        .splice(0, pending.length)
        .sort((a, b) => {
          const an = a.split('/').pop() ?? a;
          const bn = b.split('/').pop() ?? b;
          return an < bn ? -1 : an > bn ? 1 : 0;
        });
      for (const filepath of batch) {
        await handleOne(filepath);
      }
    }
  }

  if (watcher !== null) {
    watcher.on('add', (filepath) => {
      const filename = filepath.split('/').pop() ?? '';
      if (!isDeliverable(filename)) {
        // Non-`.md` files, dotfiles, prefix-sibling attachments, and
        // path-traversal names are skipped. handleOne would also skip
        // these; short-circuit to avoid adding non-deliverable paths
        // to `seen` (the poll backstop would then also skip).
        return;
      }
      // Dedup vs. the poll backstop: only the first observer of a
      // filename enqueues. `add` won't fire twice for the same file
      // on chokidar's side, so a hit here means the backstop already
      // enqueued for it.
      if (seen.has(filename)) {
        debugLog(`chokidar add (already-seen dedup): ${filename}`);
        return;
      }
      seen.add(filename);
      debugLog(`chokidar add: ${filename}`);
      enqueue(filepath);
    });
  }

  // brief-020 (HB-4) polling backstop. Runs in ADDITION to chokidar
  // (unless chokidar is disabled for tests) — a defensive scan that
  // catches inbox files whose FSEvents notification never arrived at
  // chokidar. The FSEvents backend on macOS can silently stop
  // delivering events on a long-idle process (dispatch source
  // suspension under memory pressure / long inactivity), which
  // otherwise leaves a delivered coord message wedged in the agent's
  // inbox with no channel notification ever fired. With this backstop,
  // the worst-case delay from "message hits inbox" to "agent gets
  // channel notification" is bounded by `pollBackstopMs` even when
  // chokidar is completely dead.
  let pollTimer: ReturnType<typeof setInterval> | undefined;
  function pollBackstopTick(): void {
    let entries: string[];
    try {
      entries = readdirSync(inboxDir);
    } catch {
      // inbox missing / unreadable — nothing to do this tick.
      return;
    }
    let discovered = 0;
    for (const name of entries) {
      if (!isDeliverable(name)) continue;
      if (seen.has(name)) continue;
      seen.add(name);
      discovered += 1;
      enqueue(join(inboxDir, name));
    }
    if (discovered > 0) {
      debugLog(
        `poll backstop discovered ${discovered} file(s) chokidar missed`
      );
    }
  }
  function startPollBackstop(): void {
    if (pollBackstopMs <= 0) return;
    pollTimer = setInterval(pollBackstopTick, pollBackstopMs);
    // Never block process exit for a backstop tick.
    pollTimer.unref?.();
  }
  function stopPollBackstop(): void {
    if (pollTimer !== undefined) {
      clearInterval(pollTimer);
      pollTimer = undefined;
    }
  }

  async function handleOne(filepath: string): Promise<void> {
    try {
      const filename = filepath.split('/').pop() ?? '';
      if (!isDeliverable(filename)) return;
      // No pre-emit sweep: sweep is a convergence operation now, not
      // transactional (see LAYOUT.md). If a byte-identical archive
      // twin exists for this inbox file, lazy-read sweep in cmdRead
      // will clean it up the next time something reads it; a spurious
      // channel emit for an already-archived message is recoverable
      // (the recipient archives the dup, lazy sweep clears the inbox
      // copy on next read). Worth it to drop the per-emit sweep tax
      // on chokidar's hot path.
      if (!existsSync(filepath)) return;
      const text = readFileSync(filepath, 'utf8');
      // Off-format `.md` files skip frontmatter interpretation: we
      // can't verify the sender's identity so `from` is overridden to
      // `outside`, and we don't try to derive a thread — outside
      // messages always start a new thread. Content is the raw file
      // text with a one-line marker so the recipient sees at a glance
      // this arrived through the outside path.
      const isOutside = !validFilename(filename);
      let from: string;
      let inReplyTo: string | undefined;
      let content: string;
      if (isOutside) {
        from = 'outside';
        inReplyTo = undefined;
        content = `[outside .md — non-canonical filename: ${filename}]\n\n${text}`;
      } else {
        const { fm, body } = parseFrontmatter(text);
        from =
          typeof fm.from === 'string' && fm.from.length > 0
            ? fm.from
            : 'unknown';
        const inReplyToRaw =
          typeof fm['in-reply-to'] === 'string'
            ? fm['in-reply-to']
            : typeof fm['inReplyTo'] === 'string'
              ? (fm['inReplyTo'] as string)
              : undefined;
        // Only treat in-reply-to as a thread root if it parses as a
        // LAYOUT filename; otherwise the message starts a new thread.
        inReplyTo =
          inReplyToRaw !== undefined && validFilename(inReplyToRaw)
            ? inReplyToRaw
            : undefined;
        const subject =
          typeof fm.subject === 'string' && fm.subject.length > 0
            ? fm.subject
            : undefined;
        content =
          subject !== undefined ? `Subject: ${subject}\n\n${body}` : body;
      }
      const meta = {
        from,
        messageFilename: filename,
        threadFilename: inReplyTo ?? filename,
        identity: opts.identity,
      };
      try {
        await opts.mcp.server.notification({
          method: 'notifications/claude/channel',
          params: { content, meta },
        });
        debugLog(`notification sent: ${filename} (from=${from})`);
      } catch {
        // Non-fatal: if the transport closes while we're processing
        // an in-flight add, drop the notification.
      }
    } catch {
      // A single bad file shouldn't tear down the watcher.
    }
  }

  // Wait for chokidar to reach ready (if enabled) so callers can drop
  // a file immediately after startChannelWatcher returns. With
  // chokidar disabled we're ready immediately.
  if (watcher !== null) {
    await new Promise<void>((resolve) => {
      watcher.once('ready', () => resolve());
    });
  }
  // At-least-once initial delivery: enqueue every deliverable file
  // present when the watcher started, in chronological filename
  // order. The <unix-ms>-<rand6>.md grammar's <unix-ms> prefix makes
  // a lexical sort chronological; rand6 breaks same-millisecond ties
  // deterministically. Each file was already added to `seen` above,
  // so a chokidar `add` event racing this scan or the backstop's
  // first tick will short-circuit at the `seen.has(filename)` gate
  // and not re-fire.
  if (initialFilenames.length > 0) {
    const sorted = [...initialFilenames].sort();
    for (const name of sorted) {
      enqueue(join(inboxDir, name));
    }
  }
  startPollBackstop();

  let closed = false;
  return {
    async close(): Promise<void> {
      if (closed) return;
      closed = true;
      stopPollBackstop();
      if (watcher !== null) await watcher.close();
    },
  };
}
