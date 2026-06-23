// tests/integration/mcp-channel-watcher.test.ts — chokidar-driven
// inbox watcher emitting `notifications/claude/channel`.
//
// Lives in the integration pool (single-fork) because chokidar's FS
// event subsystem flakes badly under vitest's parallel-thread unit
// pool: many short-lived watchers contend for FSEvents on macOS,
// occasionally dropping inbox-add events. The behavior under test
// only ever runs single-watcher in production.
//
// In-memory transport pair + fallbackNotificationHandler on the
// client captures every emitted notification. Each test drops one or
// more files into the watched inbox and awaits the matching
// notifications.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { createMcpServer } from '../../src/mcp/index.ts';
import { asIdentity } from '../../src/types.ts';

interface ChannelNotification {
  method: 'notifications/claude/channel';
  params: {
    content: string;
    meta: {
      from: string;
      messageFilename: string;
      threadFilename: string;
      identity: string;
    };
  };
}

let scratch: string;
let coordRoot: string;
let inbox: string;
let client: Client;
let handle: ReturnType<typeof createMcpServer>;
let received: ChannelNotification[];

beforeEach(async () => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-mcp-cwatch-'));
  coordRoot = join(scratch, 'coord');
  for (const id of ['alice', 'bob']) {
    mkdirSync(join(coordRoot, id, 'inbox'), { recursive: true });
    mkdirSync(join(coordRoot, id, 'archive'), { recursive: true });
  }
  inbox = join(coordRoot, 'alice', 'inbox');

  handle = createMcpServer({
    root: coordRoot,
    identity: asIdentity('alice'),
    channel: true,
    // Force polling: vitest's parallel pool starves macOS FSEvents
    // under concurrent watcher churn, dropping inbox-add events.
    channelWatcherOptions: { usePolling: true, pollInterval: 20 },
  });
  client = new Client({ name: 'test-cwatch', version: '1.0' });
  received = [];
  client.fallbackNotificationHandler = async (n) => {
    if (n.method === 'notifications/claude/channel') {
      received.push(n as unknown as ChannelNotification);
    }
  };
  const [c, s] = InMemoryTransport.createLinkedPair();
  await Promise.all([client.connect(c), handle.mcp.connect(s)]);
  await handle.startChannelWatcher();
});
afterEach(async () => {
  await handle.close();
  rmSync(scratch, { recursive: true, force: true });
});

/** Wait until `pred(received)` is true or `timeoutMs` elapses.
 * 6s budget tolerates the worst-case full-suite contention (many
 * chokidar watchers racing for FSEvents on macOS); locally each
 * notification round-trip is ~50–100ms. */
async function waitFor(
  pred: () => boolean,
  timeoutMs = 6000
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

function plant(filename: string, fm: Record<string, string>, body: string): void {
  const head = Object.entries(fm)
    .map(([k, v]) => `${k}: ${v}`)
    .join('\n');
  writeFileSync(join(inbox, filename), `---\n${head}\n---\n${body}\n`);
}

// ─── Notification shape ───────────────────────────────────────────────

describe('channel-watcher — notification shape', () => {
  it('a new inbox file fires notifications/claude/channel with content + meta', async () => {
    plant(
      '1714826789010-aaaaaa.md',
      { from: 'bob', subject: 'hi' },
      'the body'
    );
    await waitFor(() => received.length === 1);
    const n = received[0]!;
    expect(n.method).toBe('notifications/claude/channel');
    expect(n.params.content).toBe('Subject: hi\n\nthe body\n');
    expect(n.params.meta).toMatchObject({
      from: 'bob',
      messageFilename: '1714826789010-aaaaaa.md',
      threadFilename: '1714826789010-aaaaaa.md',
      identity: 'alice',
    });
  });

  it('content omits the Subject prefix when no subject is set', async () => {
    plant('1714826789010-aaaaaa.md', { from: 'bob' }, 'plain body');
    await waitFor(() => received.length === 1);
    expect(received[0]!.params.content).toBe('plain body\n');
  });

  it('in-reply-to populates threadFilename (different from messageFilename)', async () => {
    plant(
      '1714826789020-bbbbbb.md',
      { from: 'bob', 'in-reply-to': '1714826789010-aaaaaa.md' },
      'reply'
    );
    await waitFor(() => received.length === 1);
    expect(received[0]!.params.meta.threadFilename).toBe(
      '1714826789010-aaaaaa.md'
    );
    expect(received[0]!.params.meta.messageFilename).toBe(
      '1714826789020-bbbbbb.md'
    );
  });

  it('absent in-reply-to → threadFilename === messageFilename (new thread)', async () => {
    plant('1714826789030-cccccc.md', { from: 'bob' }, 'fresh thread');
    await waitFor(() => received.length === 1);
    const m = received[0]!.params.meta;
    expect(m.threadFilename).toBe(m.messageFilename);
  });
});

// ─── Multiple files ───────────────────────────────────────────────────

describe('channel-watcher — multiple files', () => {
  it('two consecutive files produce two notifications', async () => {
    plant('1714826789040-dddddd.md', { from: 'bob' }, 'first');
    await new Promise((r) => setTimeout(r, 5));
    plant('1714826789050-eeeeee.md', { from: 'bob' }, 'second');
    await waitFor(() => received.length === 2);
    const fns = received.map((n) => n.params.meta.messageFilename);
    expect(fns).toEqual([
      '1714826789040-dddddd.md',
      '1714826789050-eeeeee.md',
    ]);
  });

  it('a burst of 4 files is delivered in chronological filename order', async () => {
    // Plant out of chronological order to prove the watcher sorts.
    // Each file's <unix-ms> prefix is strictly increasing; a naive
    // FS-order delivery would interleave with the planting order.
    plant('1714826789200-cccccc.md', { from: 'bob' }, 'three');
    plant('1714826789100-bbbbbb.md', { from: 'bob' }, 'two');
    plant('1714826789300-dddddd.md', { from: 'bob' }, 'four');
    plant('1714826789000-aaaaaa.md', { from: 'bob' }, 'one');
    await waitFor(() => received.length === 4);
    const fns = received.map((n) => n.params.meta.messageFilename);
    expect(fns).toEqual([
      '1714826789000-aaaaaa.md',
      '1714826789100-bbbbbb.md',
      '1714826789200-cccccc.md',
      '1714826789300-dddddd.md',
    ]);
  });
});

// ─── Untyped / malformed files ────────────────────────────────────────

describe('channel-watcher — frontmatter edge cases', () => {
  it('file without frontmatter still fires; meta.from === "unknown"', async () => {
    writeFileSync(
      join(inbox, '1714826789060-ffffff.md'),
      'no fence here, just words\n'
    );
    await waitFor(() => received.length === 1);
    expect(received[0]!.params.meta.from).toBe('unknown');
    expect(received[0]!.params.content).toBe('no fence here, just words\n');
  });

  it('file with frontmatter but no `from` → meta.from === "unknown"', async () => {
    writeFileSync(
      join(inbox, '1714826789070-gggggg.md'),
      '---\nsubject: orphan\n---\nbody\n'
    );
    await waitFor(() => received.length === 1);
    expect(received[0]!.params.meta.from).toBe('unknown');
    expect(received[0]!.params.content).toBe('Subject: orphan\n\nbody\n');
  });

  it('non-LAYOUT filename (no <unix-ms>-<rand6>.md grammar) is ignored', async () => {
    writeFileSync(join(inbox, 'random.md'), 'whatever\n');
    // Give chokidar a moment to (not) fire.
    await new Promise((r) => setTimeout(r, 150));
    expect(received).toHaveLength(0);
  });
});

// ─── Channel emit semantics with an archive twin present ─────────────
//
// Post sweep-as-convergence: the watcher does NOT presweep on emit. A
// byte-identical inbox/archive twin will fire a (spurious) notification;
// the recipient's lazy-read sweep cleans up on the next read. Divergent
// twins still fire as before — the inbox version is the new arrival.

describe('channel-watcher — archive twin behavior', () => {
  it('byte-identical inbox+archive twin → notification STILL fires (no pre-emit sweep)', async () => {
    const filename = '1714826789080-hhhhhh.md';
    const bytes = '---\nfrom: bob\n---\nzombie\n';
    writeFileSync(join(coordRoot, 'alice', 'archive', filename), bytes);
    writeFileSync(join(inbox, filename), bytes);
    // Spurious notification will fire; recipient's lazy-read sweep
    // is responsible for cleaning up the inbox copy on next access.
    await waitFor(() => received.length === 1);
    expect(received[0]!.params.meta.messageFilename).toBe(filename);
  });

  it('divergent inbox/archive copies → notification fires for the inbox version', async () => {
    const filename = '1714826789090-iiiiii.md';
    writeFileSync(
      join(coordRoot, 'alice', 'archive', filename),
      '---\nfrom: bob\n---\narchived version\n'
    );
    writeFileSync(
      join(inbox, filename),
      '---\nfrom: bob\n---\ninbox version\n'
    );
    await waitFor(() => received.length === 1);
    expect(received[0]!.params.content).toBe('inbox version\n');
  });
});

// ─── Cleanup ──────────────────────────────────────────────────────────

describe('channel-watcher — cleanup', () => {
  it('startChannelWatcher is idempotent (second call is a no-op)', async () => {
    await handle.startChannelWatcher();
    await handle.startChannelWatcher();
    plant('1714826789100-jjjjjj.md', { from: 'bob' }, 'idempotent');
    await waitFor(() => received.length === 1);
  });

  it('close() disposes the watcher; no notifications fire after close', async () => {
    await handle.close();
    plant('1714826789110-kkkkkk.md', { from: 'bob' }, 'post-close');
    await new Promise((r) => setTimeout(r, 150));
    expect(received).toHaveLength(0);
    // close() must remain idempotent.
    await expect(handle.close()).resolves.not.toThrow();
  });
});

// ─── Lifecycle: runWith resolves on transport close ───────────────────
//
// Standalone describe block so it doesn't share the global `handle` /
// `client` / etc. — `runWith` owns its own transport and we close the
// pair ourselves.

describe('channel-watcher — runWith lifecycle', () => {
  let scratch2: string;
  let coordRoot2: string;

  beforeEach(() => {
    scratch2 = mkdtempSync(join(tmpdir(), 'coord-mcp-rw-'));
    coordRoot2 = join(scratch2, 'coord');
    for (const id of ['alice', 'bob']) {
      mkdirSync(join(coordRoot2, id, 'inbox'), { recursive: true });
      mkdirSync(join(coordRoot2, id, 'archive'), { recursive: true });
    }
  });
  afterEach(() => {
    rmSync(scratch2, { recursive: true, force: true });
  });

  it('host closing the transport → runWith resolves and the watcher is torn down', async () => {
    const h = createMcpServer({
      root: coordRoot2,
      identity: asIdentity('alice'),
      channel: true,
      channelWatcherOptions: { usePolling: true, pollInterval: 20 },
    });
    const c = new Client({ name: 'rw-test', version: '1.0' });
    const seen: ChannelNotification[] = [];
    c.fallbackNotificationHandler = async (n) => {
      if (n.method === 'notifications/claude/channel') {
        seen.push(n as unknown as ChannelNotification);
      }
    };
    const [clientTransport, serverTransport] =
      InMemoryTransport.createLinkedPair();
    // Start runWith in the background BEFORE connecting the client.
    // runWith awaits mcp.connect(serverTransport); the client must be
    // connecting concurrently for the initialize handshake to complete.
    const runPromise = h.runWith(serverTransport);
    await c.connect(clientTransport);
    // Wait for runWith's setup (mcp.connect + chokidar.ready) to
    // complete. startChannelWatcher is idempotent and resolves once
    // the chokidar watcher is ready — this is a clean "setup is
    // done" signal regardless of how fast mcp.connect resolves.
    await h.startChannelWatcher();

    // Sanity: the watcher fires before close.
    writeFileSync(
      join(coordRoot2, 'alice', 'inbox', '1714826789010-aaaaaa.md'),
      '---\nfrom: bob\n---\nbefore close\n'
    );
    const deadline = Date.now() + 3000;
    while (seen.length === 0 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 25));
    }
    expect(seen).toHaveLength(1);

    // Host closes its end of the pair; the server side's transport
    // onclose propagates up through Protocol → mcp.server.onclose →
    // our `settle` callback in runWith.
    await c.close();

    // runWith should resolve within a generous timeout. If the bug
    // returns, this `await` hangs and vitest's per-test timeout fires.
    await Promise.race([
      runPromise,
      new Promise((_, reject) =>
        setTimeout(
          () => reject(new Error('runWith did not resolve within 3000ms')),
          3000
        )
      ),
    ]);

    // After runWith resolves, close() should clean up the watcher.
    // No further notifications should fire from new inbox arrivals.
    await h.close();
    writeFileSync(
      join(coordRoot2, 'alice', 'inbox', '1714826789020-bbbbbb.md'),
      '---\nfrom: bob\n---\nafter close\n'
    );
    await new Promise((r) => setTimeout(r, 200));
    expect(seen).toHaveLength(1);
  });
});
