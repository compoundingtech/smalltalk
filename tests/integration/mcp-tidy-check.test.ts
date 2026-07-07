// tests/integration/mcp-tidy-check.test.ts — tidy-check tick wired
// into the MCP server (brief-030 task 2).
//
// Uses an in-memory transport pair + fallbackNotificationHandler to
// capture every synthetic `notifications/claude/channel` emit. Each
// test plants drift, advances the timer (small interval via
// tidyCheckIntervalMs), and asserts the emit shape + dedup behavior.
//
// Lives in the integration pool (single-fork) so the polling-style
// timer doesn't race against other tests' setInterval churn.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import {
  mkdirSync,
  mkdtempSync,
  rmSync,
  utimesSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { STALE_INBOX_MS } from '../../src/common.ts';
import { createMcpServer } from '../../src/mcp/index.ts';
import { asIdentity } from '../../src/types.ts';

interface TidyNotification {
  method: 'notifications/claude/channel';
  params: {
    content: string;
    meta: {
      from: string;
      kind?: string;
      identity: string;
      messageFilename: string | null;
      threadFilename: string | null;
    };
  };
}

const ID = 'alice';

let scratch: string;
let stRoot: string;
let client: Client;
let handle: ReturnType<typeof createMcpServer>;
let run: Promise<void> | undefined;
let received: TidyNotification[];

async function boot(opts: { tidyCheckIntervalMs?: number; channel?: boolean } = {}): Promise<void> {
  handle = createMcpServer({
    root: stRoot,
    identity: asIdentity(ID),
    channel: opts.channel ?? true,
    channelWatcherOptions: { usePolling: true, pollInterval: 50 },
    // Default to a small tick so tests run in test-time.
    tidyCheckIntervalMs: opts.tidyCheckIntervalMs ?? 50,
    // Keep status-refresh out of the way for these tests.
    statusRefreshIntervalMs: 0,
  });
  client = new Client({ name: 'test-tidy', version: '1.0' });
  received = [];
  client.fallbackNotificationHandler = async (n) => {
    // Filter to tidy-check frames only — chokidar's inbox-arrival
    // watcher uses the same method name, so we'd otherwise count a
    // real `st_msg_send`-shaped notification as a tidy emit when
    // a planted file shows up under the watched inbox.
    if (n.method !== 'notifications/claude/channel') return;
    const meta = (
      n.params as { meta?: { kind?: string } } | undefined
    )?.meta;
    if (meta?.kind === 'tidy-check') {
      received.push(n as unknown as TidyNotification);
    }
  };
  const [c, s] = InMemoryTransport.createLinkedPair();
  // Drive lifecycle through runWith so startTidyCheck fires.
  // runWith awaits until the transport closes; capture the promise
  // without awaiting and clean up in afterEach.
  run = handle.runWith(s);
  await client.connect(c);
}

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-mcp-tidy-'));
  stRoot = join(scratch, 'coord');
  mkdirSync(join(stRoot, ID, 'inbox'), { recursive: true });
  mkdirSync(join(stRoot, ID, 'archive'), { recursive: true });
  // Mark identity as `available` so the gate doesn't suppress emits
  // by default. Tests that exercise busy/dnd/unknown overwrite this.
  writeFileSync(join(stRoot, ID, 'status'), 'available\n');
  received = [];
});

afterEach(async () => {
  if (client) {
    try {
      await client.close();
    } catch {
      // ignore
    }
  }
  if (handle) await handle.close();
  if (run !== undefined) {
    try {
      await run;
    } catch {
      // ignore
    }
    run = undefined;
  }
  rmSync(scratch, { recursive: true, force: true });
});

async function waitFor(
  pred: () => boolean,
  timeoutMs = 4000
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (pred()) return;
    await new Promise((r) => setTimeout(r, 25));
  }
  throw new Error(
    `waitFor: predicate did not become true within ${timeoutMs}ms; received=${JSON.stringify(received)}`
  );
}

function plantInboxStale(filename: string): void {
  const path = join(stRoot, ID, 'inbox', filename);
  writeFileSync(path, '---\nfrom: bob\n---\nbody\n');
  const t = new Date(Date.now() - STALE_INBOX_MS - 60_000);
  utimesSync(path, t, t);
}

// ─── Basic firing ──────────────────────────────────────────────────────

describe('tidy-check tick — fires on drift', () => {
  it('plants stale inbox → emits a tidy-check notification', async () => {
    plantInboxStale('1714826789010-aaaaaa.md');
    await boot({ tidyCheckIntervalMs: 50 });
    await waitFor(() => received.length >= 1);
    const n = received[0]!;
    expect(n.params.meta.kind).toBe('tidy-check');
    expect(n.params.meta.from).toBe('coord-system');
    expect(n.params.meta.messageFilename).toBeNull();
    expect(n.params.meta.threadFilename).toBeNull();
    expect(n.params.meta.identity).toBe(ID);
    expect(n.params.content).toContain('Tidy check (drift detected)');
    expect(n.params.content).toContain('inbox:');
  });

  it('no drift across a several-tick window → no emit', async () => {
    // No inbox files planted; identity is `available`.
    await boot({ tidyCheckIntervalMs: 30 });
    // Give the timer a generous window to fire if it were going to.
    await new Promise((r) => setTimeout(r, 300));
    expect(received).toEqual([]);
  });
});

// ─── Dedup ─────────────────────────────────────────────────────────────

describe('tidy-check tick — dedup behavior', () => {
  it('same drift across multiple ticks → only one emit', async () => {
    plantInboxStale('1714826789010-aaaaaa.md');
    await boot({ tidyCheckIntervalMs: 30 });
    // Wait long enough that multiple ticks would have fired.
    await new Promise((r) => setTimeout(r, 250));
    expect(received).toHaveLength(1);
  });

  it('drift clears then re-emerges → second emit', async () => {
    const filename = '1714826789010-aaaaaa.md';
    plantInboxStale(filename);
    await boot({ tidyCheckIntervalMs: 30 });
    await waitFor(() => received.length >= 1);

    // Clear the inbox.
    rmSync(join(stRoot, ID, 'inbox', filename));
    // Give the tick a beat to observe "no drift" and update lastFired.
    await new Promise((r) => setTimeout(r, 100));

    // Drift re-emerges → expect a second emit.
    plantInboxStale('1714826789020-bbbbbb.md');
    await waitFor(() => received.length >= 2);
    expect(received).toHaveLength(2);
  });
});

// ─── Status gate ───────────────────────────────────────────────────────

describe('tidy-check tick — status gate', () => {
  async function withStatus(status: string): Promise<void> {
    writeFileSync(join(stRoot, ID, 'status'), `${status}\n`);
  }

  it('status: busy → tick is a no-op (no emit)', async () => {
    plantInboxStale('1714826789010-aaaaaa.md');
    await withStatus('busy');
    await boot({ tidyCheckIntervalMs: 30 });
    await new Promise((r) => setTimeout(r, 200));
    expect(received).toEqual([]);
  });

  it('status: dnd → no emit', async () => {
    plantInboxStale('1714826789010-aaaaaa.md');
    await withStatus('dnd');
    await boot({ tidyCheckIntervalMs: 30 });
    await new Promise((r) => setTimeout(r, 200));
    expect(received).toEqual([]);
  });

  it('status: away → emit still fires (away does NOT suppress)', async () => {
    plantInboxStale('1714826789010-aaaaaa.md');
    await withStatus('away');
    await boot({ tidyCheckIntervalMs: 30 });
    await waitFor(() => received.length >= 1);
  });

  it('busy → flip to available → next eligible tick emits the accumulated drift', async () => {
    plantInboxStale('1714826789010-aaaaaa.md');
    await withStatus('busy');
    await boot({ tidyCheckIntervalMs: 30 });
    await new Promise((r) => setTimeout(r, 150));
    expect(received).toEqual([]);

    // Flip back; the gate stops returning early, and since lastFired
    // wasn't updated during busy, the drift still looks new.
    writeFileSync(join(stRoot, ID, 'status'), 'available\n');
    await waitFor(() => received.length >= 1);
  });
});

// ─── Disable / configurability ─────────────────────────────────────────

describe('tidy-check tick — configurability', () => {
  it('tidyCheckIntervalMs: 0 disables the timer', async () => {
    plantInboxStale('1714826789010-aaaaaa.md');
    await boot({ tidyCheckIntervalMs: 0 });
    await new Promise((r) => setTimeout(r, 200));
    expect(received).toEqual([]);
  });

  it('non-channel mode still receives the synthetic frame', async () => {
    // Notifications are SDK-level; channel mode just adds the
    // chokidar watcher + reply tool + experimental capability. The
    // tidy-check tick fires regardless of channel mode.
    plantInboxStale('1714826789010-aaaaaa.md');
    await boot({ tidyCheckIntervalMs: 30, channel: false });
    await waitFor(() => received.length >= 1);
  });
});
