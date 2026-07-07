// mcp/index.ts — createMcpServer factory.
//
// `st mcp` runs this server over stdio. The factory builds an
// McpServer wrapping a St instance and registers the five tools.
// The `run()` method connects to a stdio transport and awaits SIGINT,
// SIGTERM, or transport close — whichever comes first.
//
// All MCP imports are reachable only when this module is loaded — keep
// it isolated from the rest of src/ so the cost of `@modelcontextprotocol/sdk`
// is paid only by `st mcp`, never by `st send` / `ls` / etc.

import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import type { Transport } from '@modelcontextprotocol/sdk/shared/transport.js';
import { writeFileSync } from 'node:fs';

import {
  statusPath,
  STATUS_REFRESH_MS,
  TIDY_CHECK_INTERVAL_MS,
} from '../common.ts';
import {
  readIdentityStatus,
  refreshIdentityStatus,
} from '../commands/status.ts';
import { createSt, type St } from '../lib.ts';
import type { Identity } from '../types.ts';
import { buildServerInfo, buildServerOptions } from './capabilities.ts';
import { evaluateDrift } from './tidy-check.ts';
import { registerArchiveTool } from './tools/archive.ts';
import { registerContextTools } from './tools/context.ts';
import { registerLsTool } from './tools/ls.ts';
import { registerAgentsTool } from './tools/agents.ts';
import { registerReadTool } from './tools/read.ts';
import { registerReplyTool } from './tools/reply.ts';
import { registerResourceTools } from './tools/resource.ts';
import { registerSendTool } from './tools/send.ts';
import { registerThreadTool } from './tools/thread.ts';

export interface McpServerOptions {
  root: string;
  identity: Identity;
  configRoot?: string | undefined;
  /**
   * When true, advertise `experimental['claude/channel']`, attach the
   * channel-mode instructions, and (in later tasks) start the inbox
   * watcher and register `st_msg_reply`. Default: false.
   */
  channel?: boolean | undefined;
  /**
   * Test-only escape hatch passed through to chokidar. Production
   * leaves this empty (FSEvents/inotify); tests set
   * `{ usePolling: true }` so vitest's parallel pool can't starve
   * FSEvents into dropping inbox-add events.
   *
   * brief-020 adds:
   *   - `pollBackstopIntervalMs` — how often the polling backstop
   *     scans the inbox for files chokidar missed. Default 15s.
   *   - `chokidarEnabled` — test seam that disables chokidar entirely
   *     so the backstop can be exercised in isolation.
   *   - `debug` — mirror of `COORD_CHANNEL_DEBUG=1`, forwarded here so
   *     library embedders can enable the same instrumentation.
   */
  channelWatcherOptions?:
    | {
        usePolling?: boolean;
        pollInterval?: number;
        pollBackstopIntervalMs?: number;
        chokidarEnabled?: boolean;
        debug?: boolean;
      }
    | undefined;
  /**
   * How often (ms) to refresh the identity's status file mtime so
   * peers don't see this agent fall into the `unknown` staleness
   * window while it's still running. Defaults to STATUS_REFRESH_MS
   * (5 min) in production. Tests can pass a small value (e.g. 50)
   * to observe the refresh tick without waiting real minutes. Set
   * to 0 to disable the refresh entirely.
   */
  statusRefreshIntervalMs?: number | undefined;
  /**
   * brief-030: how often (ms) to run the tidy-check drift detector.
   * Defaults to TIDY_CHECK_INTERVAL_MS (20 min). Tests pass a small
   * value (e.g. 50) to observe ticks. Set to 0 to disable entirely.
   * The tick is a no-op when the agent's status is busy/dnd/unknown
   * — see the gate in runWith.
   */
  tidyCheckIntervalMs?: number | undefined;
  /**
   * brief-005-phase0: which canonical name the MCP server announces
   * itself as. `st` (legacy) or `st` (new short canonical).
   * Derived from the bash shim's `_ST_INVOKED_AS` env var; defaults
   * to `st` for back-compat with tests / direct lib embedders
   * that don't set it.
   */
  serverName?: 'st' | undefined;
}

export interface McpServerHandle {
  /** The underlying high-level server. */
  mcp: McpServer;
  /** The St instance baked into every tool handler. */
  st: St;
  /** Connect to a stdio transport and run until the transport closes
   * or the process receives SIGINT/SIGTERM. */
  run(): Promise<void>;
  /** Like {@link run} but lets the caller supply the transport. Used
   * by tests to drive the lifecycle through an in-memory transport
   * pair without spawning a subprocess. */
  runWith(transport: Transport): Promise<void>;
  /** Close the server, dispose the channel watcher (if started), and
   * release the underlying transport. Idempotent. */
  close(): Promise<void>;
  /**
   * Start the chokidar inbox watcher. No-op when channel mode is off
   * or when the watcher was already started. Returns once the watcher
   * is ready to report new arrivals. Tests using the in-memory
   * transport call this explicitly after connecting; `run()` calls it
   * implicitly before awaiting the SIGINT/SIGTERM signal.
   */
  startChannelWatcher(): Promise<void>;
}

export function createMcpServer(opts: McpServerOptions): McpServerHandle {
  const channel = opts.channel === true;
  const st = createSt({
    root: opts.root,
    identity: opts.identity,
    ...(opts.configRoot !== undefined && { configRoot: opts.configRoot }),
  });

  const serverName = opts.serverName ?? 'st';
  const mcp = new McpServer(
    buildServerInfo(serverName),
    buildServerOptions({ channel })
  );

  // Tool registration. Each register* call wires one tool over the
  // `st` instance; ordering doesn't matter (the SDK installs the
  // request handlers lazily on first registration).
  registerSendTool(mcp, st);
  registerLsTool(mcp, st);
  registerReadTool(mcp, st);
  registerArchiveTool(mcp, st);
  registerThreadTool(mcp, st);
  // st_agents (+ st_members deprecated alias) is available in
  // both modes — peer discovery is useful regardless of channel.
  registerAgentsTool(mcp, st);
  // st_resource_* (brief-009 item 5): add/ls/read/remove. Available
  // in both modes; resources are part of the always-on agent surface.
  registerResourceTools(mcp, st);
  // st_context_* (brief-024): read/write/append the per-agent
  // context/ folder for lossless-restart. Available in both modes; the
  // St's own context/ is the only writer target, but reads across
  // identities are supported for stinators inspecting a peer's
  // last-known-state.
  registerContextTools(mcp, st);
  if (channel) {
    // st_msg_reply is the channel-mode partner of the inbox watcher.
    registerReplyTool(mcp, st);
  }

  // Channel-watcher state — populated lazily on the first
  // startChannelWatcher() call.
  let watcherStart: Promise<{ close(): Promise<void> } | null> | undefined;

  async function startChannelWatcher(): Promise<void> {
    if (!channel) return;
    if (watcherStart === undefined) {
      watcherStart = (async () => {
        const { startChannelWatcher: start } = await import(
          './channel-watcher.ts'
        );
        const startOpts: Parameters<typeof start>[0] = {
          mcp,
          root: opts.root,
          identity: opts.identity,
        };
        if (opts.channelWatcherOptions?.usePolling !== undefined) {
          startOpts.usePolling = opts.channelWatcherOptions.usePolling;
        }
        if (opts.channelWatcherOptions?.pollInterval !== undefined) {
          startOpts.pollInterval = opts.channelWatcherOptions.pollInterval;
        }
        if (
          opts.channelWatcherOptions?.pollBackstopIntervalMs !== undefined
        ) {
          startOpts.pollBackstopIntervalMs =
            opts.channelWatcherOptions.pollBackstopIntervalMs;
        }
        if (opts.channelWatcherOptions?.chokidarEnabled !== undefined) {
          startOpts.chokidarEnabled =
            opts.channelWatcherOptions.chokidarEnabled;
        }
        if (opts.channelWatcherOptions?.debug !== undefined) {
          startOpts.debug = opts.channelWatcherOptions.debug;
        }
        return start(startOpts);
      })();
    }
    await watcherStart;
  }

  // brief-022 task 2: write `offline` to the status file on any
  // shutdown path so peers don't see a stale `available` after this
  // process dies. Sync writeFileSync is required for the `exit`
  // listener; signals + transport-close fire async but use the same
  // sync helper for consistency. Best-effort; a dying process must
  // not crash harder because of a failed status write. Deduped via
  // the `wroteOffline` closure flag so SIGINT-then-exit only writes
  // once.
  let wroteOffline = false;
  const writeOfflineSync = (): void => {
    if (wroteOffline) return;
    wroteOffline = true;
    if (!opts.identity || opts.identity.length === 0) return;
    try {
      writeFileSync(statusPath(opts.identity, opts.root), 'offline\n');
    } catch {
      // best-effort; the process is exiting either way
    }
  };
  // The `exit` listener is a safety net for shutdown paths that don't
  // route through `settle` (uncaught exceptions, explicit
  // process.exit() from other code). Idempotent via wroteOffline.
  const onProcessExit = (): void => writeOfflineSync();

  // brief-023: periodic mtime refresh. A healthy idle agent's status
  // file would otherwise age past STATUS_STALE_MS and surface as
  // `unknown` to peers even though the agent is alive. The refresh
  // tick re-writes the *current* recorded value (preserving user
  // intent — busy stays busy, dnd stays dnd), or writes `available`
  // if the file is missing. Corrupt or `unknown` contents are left
  // alone: we never invent a value we can't justify.
  let statusRefreshTimer: ReturnType<typeof setInterval> | undefined;
  const refreshStatus = (): void => {
    if (!opts.identity || opts.identity.length === 0) return;
    // Delegates to refreshIdentityStatus in commands/status.ts so the
    // brief-032 ding-side mirror runs the same code path. The
    // outcome enum is informational; this caller doesn't act on it
    // (best-effort, same as before).
    refreshIdentityStatus(opts.identity, opts.root);
  };
  const startStatusRefresh = (): void => {
    const ms = opts.statusRefreshIntervalMs ?? STATUS_REFRESH_MS;
    if (ms <= 0) return;
    if (!opts.identity || opts.identity.length === 0) return;
    statusRefreshTimer = setInterval(refreshStatus, ms);
    // Don't pin the event loop alive just for the refresh — the
    // server's transport-close / signal handling drives lifecycle.
    statusRefreshTimer.unref?.();
  };
  const stopStatusRefresh = (): void => {
    if (statusRefreshTimer !== undefined) {
      clearInterval(statusRefreshTimer);
      statusRefreshTimer = undefined;
    }
  };

  // brief-030: periodic tidy-check tick. Walks inbox for drift; emits
  // a synthetic `notifications/claude/channel` frame (from:
  // st-system) when drift turns from false→true since the last
  // emit. No emit when status is busy/dnd/unknown — busy/dnd defer
  // until the agent flips back; unknown means we don't know what state
  // they're in and shouldn't pile on. Dedup tracks current state, not
  // historical: a drift that clears and re-emerges fires again.
  let tidyCheckTimer: ReturnType<typeof setInterval> | undefined;
  let lastTidyFired: { inbox: boolean } = { inbox: false };
  const tidyCheck = (): void => {
    if (!opts.identity || opts.identity.length === 0) return;
    let currentStatus: string;
    try {
      currentStatus = readIdentityStatus(opts.identity, opts.root);
    } catch {
      return; // best-effort: any error → skip this tick
    }
    // Gate per the brief: busy/dnd defer, unknown skip. (No emit AND
    // no lastTidyFired update so the next eligible tick will catch up
    // on whatever drifted while we were gated.)
    if (
      currentStatus === 'busy' ||
      currentStatus === 'dnd' ||
      currentStatus === 'unknown'
    ) {
      return;
    }
    let drift: ReturnType<typeof evaluateDrift>;
    try {
      drift = evaluateDrift(opts.identity, opts.root);
    } catch {
      return; // best-effort
    }
    // New-condition test: drift went false→true compared to last
    // fired. Same-or-less drift doesn't re-emit.
    const newCondition = drift.inbox && !lastTidyFired.inbox;
    if (newCondition && drift.body.length > 0) {
      void mcp.server
        .notification({
          method: 'notifications/claude/channel',
          params: {
            content: drift.body,
            meta: {
              from: 'st-system',
              kind: 'tidy-check',
              identity: opts.identity,
              messageFilename: null,
              threadFilename: null,
            },
          },
        })
        .catch(() => {
          // Transport may have closed mid-flight; non-fatal.
        });
    }
    // Update lastTidyFired on every eligible tick (not just emits)
    // so a drift that clears stops looking "new" on its next
    // recurrence — only the recurrence-after-clear fires.
    lastTidyFired = { inbox: drift.inbox };
  };
  const startTidyCheck = (): void => {
    const ms = opts.tidyCheckIntervalMs ?? TIDY_CHECK_INTERVAL_MS;
    if (ms <= 0) return;
    if (!opts.identity || opts.identity.length === 0) return;
    tidyCheckTimer = setInterval(tidyCheck, ms);
    tidyCheckTimer.unref?.();
  };
  const stopTidyCheck = (): void => {
    if (tidyCheckTimer !== undefined) {
      clearInterval(tidyCheckTimer);
      tidyCheckTimer = undefined;
    }
  };

  async function runWith(transport: Transport): Promise<void> {
    await mcp.connect(transport);
    await startChannelWatcher();
    startStatusRefresh();
    startTidyCheck();
    process.on('exit', onProcessExit);
    await new Promise<void>((resolve) => {
      let resolved = false;
      const settle = (): void => {
        if (resolved) return;
        resolved = true;
        // Stop the refresh tick *before* writing offline so we don't
        // race the timer overwriting the offline value back to
        // `available`. Order matters: stop, then write.
        stopStatusRefresh();
        // Same race-avoidance for the tidy-check tick — clear before
        // teardown so a tick in flight can't fire a synthetic
        // notification on a transport that's about to close.
        stopTidyCheck();
        // Write `offline` before any further teardown so peers see
        // the right status as quickly as possible. Sync I/O is fine
        // here; the process is shutting down.
        writeOfflineSync();
        process.removeListener('SIGINT', settle);
        process.removeListener('SIGTERM', settle);
        // Drop our onclose hook so a later close() doesn't fire it
        // again. Setting back to undefined is fine — Protocol's _onclose
        // checks for the presence of the callback before invoking it.
        mcp.server.onclose = undefined;
        resolve();
      };
      // Three exit paths, all funnel into `settle`:
      //   - the host closes the transport (stdio EOF, in-memory pair
      //     teardown, or any other Transport.onclose firing);
      //   - SIGINT (Ctrl-C);
      //   - SIGTERM (process manager).
      // mcp.server.onclose is invoked by Protocol when the transport
      // closes for any reason — including our own close() call.
      mcp.server.onclose = settle;
      process.once('SIGINT', settle);
      process.once('SIGTERM', settle);
    });
  }

  return {
    mcp,
    st: st,
    runWith,
    async run(): Promise<void> {
      await runWith(new StdioServerTransport());
    },
    async close(): Promise<void> {
      // Drop the exit safety-net listener so test runs that
      // create+close many handles don't accumulate listeners on
      // process.
      process.removeListener('exit', onProcessExit);
      stopStatusRefresh();
      stopTidyCheck();
      if (watcherStart !== undefined) {
        try {
          const w = await watcherStart;
          if (w !== null) await w.close();
        } catch {
          // best-effort: a failed watcher start shouldn't block close.
        }
        watcherStart = undefined;
      }
      await mcp.close();
    },
    startChannelWatcher,
  };
}
