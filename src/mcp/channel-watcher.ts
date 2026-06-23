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
  const { existsSync, readFileSync } = await import('node:fs');
  const { join } = await import('node:path');
  const { parseFrontmatter, validFilename } = await import(
    '../common.ts'
  );

  const inboxDir = join(opts.root, opts.identity, 'inbox');

  const watcher = chokidar.watch(inboxDir, {
    ignoreInitial: true,
    persistent: true,
    // No awaitWriteFinish: every coord producer (coord.send,
    // tmp+atomic-rename; rsync, atomic-on-completion) writes a single
    // final file. Polling for write stability just adds latency that
    // makes the watcher flake on heavily-loaded test runs.
    ...(opts.usePolling === true && {
      usePolling: true,
      interval: opts.pollInterval ?? 50,
      binaryInterval: opts.pollInterval ?? 50,
    }),
  });

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

  watcher.on('add', (filepath) => enqueue(filepath));

  async function handleOne(filepath: string): Promise<void> {
    try {
      const filename = filepath.split('/').pop() ?? '';
      if (!validFilename(filename)) return;
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
      const { fm, body } = parseFrontmatter(text);
      const from =
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
      const inReplyTo =
        inReplyToRaw !== undefined && validFilename(inReplyToRaw)
          ? inReplyToRaw
          : undefined;
      const subject =
        typeof fm.subject === 'string' && fm.subject.length > 0
          ? fm.subject
          : undefined;
      const content =
        subject !== undefined ? `Subject: ${subject}\n\n${body}` : body;
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
      } catch {
        // Non-fatal: if the transport closes while we're processing
        // an in-flight add, drop the notification.
      }
    } catch {
      // A single bad file shouldn't tear down the watcher.
    }
  }

  await new Promise<void>((resolve) => {
    watcher.once('ready', () => resolve());
  });

  let closed = false;
  return {
    async close(): Promise<void> {
      if (closed) return;
      closed = true;
      await watcher.close();
    },
  };
}
