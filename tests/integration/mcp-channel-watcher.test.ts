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
let stRoot: string;
let inbox: string;
let client: Client;
let handle: ReturnType<typeof createMcpServer>;
let received: ChannelNotification[];

beforeEach(async () => {
  scratch = mkdtempSync(join(tmpdir(), 'st-mcp-cwatch-'));
  stRoot = join(scratch, 'smalltalk');
  for (const id of ['alice', 'bob']) {
    mkdirSync(join(stRoot, id, 'inbox'), { recursive: true });
    mkdirSync(join(stRoot, id, 'archive'), { recursive: true });
  }
  inbox = join(stRoot, 'alice', 'inbox');

  handle = createMcpServer({
    root: stRoot,
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

  it('non-.md filename (e.g. README) is ignored', async () => {
    writeFileSync(join(inbox, 'README'), 'not a message\n');
    writeFileSync(join(inbox, '.hidden.md'), 'dotfile\n');
    // Give chokidar a moment to (not) fire.
    await new Promise((r) => setTimeout(r, 150));
    expect(received).toHaveLength(0);
  });
});

// ─── Outside .md delivery (task #128 — kill the silent miss) ────────
//
// A `.md` file dropped into inbox/ that doesn't match the
// LAYOUT-004 `<unix-ms>-<rand6>.md` grammar used to be silently
// ignored. Now it fires as an "outside" message so the recipient
// sees a collaborator's hand-dropped `.md` instead of a wedged
// inbox.

describe('channel-watcher — outside .md delivery', () => {
  it('fires notifications/claude/channel with from="outside" for a bare `.md` filename', async () => {
    writeFileSync(
      join(inbox, 'random.md'),
      '---\nfrom: alice\nsubject: forged?\n---\nthe body\n'
    );
    await waitFor(() => received.length === 1);
    const n = received[0]!;
    expect(n.params.meta.from).toBe('outside');
    // The recipient's tool for replying (`st_msg_reply`) can't
    // build a thread from an outside filename — always a new thread.
    expect(n.params.meta.messageFilename).toBe('random.md');
    expect(n.params.meta.threadFilename).toBe('random.md');
    expect(n.params.content).toMatch(/^\[outside \.md — non-canonical filename: random\.md\]/);
    // Even a well-formed `from:` in the frontmatter is not trusted:
    // the file's non-canonical name means we can't verify the sender.
    expect(n.params.content).not.toContain('Subject: forged?');
  });

  it('a plain-body outside .md (no frontmatter) also fires', async () => {
    writeFileSync(join(inbox, 'notes.md'), 'just plain body\n');
    await waitFor(() => received.length === 1);
    expect(received[0]!.params.meta.from).toBe('outside');
    expect(received[0]!.params.content).toContain('just plain body\n');
  });

  it('prefix-sibling attachment (`<ts>-<r6>.subject.md`) is NOT delivered', async () => {
    // Belongs to a canonical .md; delivering it as outside would
    // double-fire when the canonical arrives.
    writeFileSync(
      join(inbox, '1714826789150-abc123.subject.md'),
      'sidecar\n'
    );
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
    writeFileSync(join(stRoot, 'alice', 'archive', filename), bytes);
    writeFileSync(join(inbox, filename), bytes);
    // Spurious notification will fire; recipient's lazy-read sweep
    // is responsible for cleaning up the inbox copy on next access.
    await waitFor(() => received.length === 1);
    expect(received[0]!.params.meta.messageFilename).toBe(filename);
  });

  it('divergent inbox/archive copies → notification fires for the inbox version', async () => {
    const filename = '1714826789090-iiiiii.md';
    writeFileSync(
      join(stRoot, 'alice', 'archive', filename),
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

// ─── brief-020 (HB-4): polling backstop for missed FSEvents ──────────
//
// The channel-watcher runs a `setInterval` scan of the inbox in
// ADDITION to chokidar — a defensive layer that catches inbox files
// FSEvents may have silently dropped (a real class of macOS bug that
// wedges idle Claude Code agents). These tests exercise the backstop
// in isolation via the `chokidarEnabled: false` seam: chokidar never
// starts, so any notification that fires MUST have come from the poll.

describe('channel-watcher — poll backstop (brief-020 HB-4)', () => {
  let backstopScratch: string;
  let backstopRoot: string;
  let backstopInbox: string;
  let backstopHandle: ReturnType<typeof createMcpServer>;
  let backstopClient: Client;
  let backstopReceived: ChannelNotification[];

  beforeEach(async () => {
    backstopScratch = mkdtempSync(join(tmpdir(), 'st-mcp-cwatch-bs-'));
    backstopRoot = join(backstopScratch, 'smalltalk');
    for (const id of ['alice', 'bob']) {
      mkdirSync(join(backstopRoot, id, 'inbox'), { recursive: true });
      mkdirSync(join(backstopRoot, id, 'archive'), { recursive: true });
    }
    backstopInbox = join(backstopRoot, 'alice', 'inbox');
    backstopHandle = createMcpServer({
      root: backstopRoot,
      identity: asIdentity('alice'),
      channel: true,
      channelWatcherOptions: {
        // Force chokidar OFF so any received notification proves the
        // backstop is the delivery path. Tests must not be able to
        // false-pass via chokidar.
        chokidarEnabled: false,
        // Aggressive poll interval — a real 15s backstop would blow
        // the test budget. Small enough that a single tick fires
        // within our waitFor window.
        pollBackstopIntervalMs: 50,
      },
    });
    backstopClient = new Client({ name: 'test-cwatch-bs', version: '1.0' });
    backstopReceived = [];
    backstopClient.fallbackNotificationHandler = async (n) => {
      if (n.method === 'notifications/claude/channel') {
        backstopReceived.push(n as unknown as ChannelNotification);
      }
    };
    const [c, s] = InMemoryTransport.createLinkedPair();
    await Promise.all([
      backstopClient.connect(c),
      backstopHandle.mcp.connect(s),
    ]);
    await backstopHandle.startChannelWatcher();
  });
  afterEach(async () => {
    await backstopHandle.close();
    rmSync(backstopScratch, { recursive: true, force: true });
  });

  async function waitForBackstop(
    pred: () => boolean,
    timeoutMs = 3000
  ): Promise<void> {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (pred()) return;
      await new Promise((r) => setTimeout(r, 25));
    }
    throw new Error(
      `waitForBackstop: predicate did not become true within ${timeoutMs}ms; received=${JSON.stringify(backstopReceived)}`
    );
  }

  it('backstop-only: file appearing after startup fires a channel notification', async () => {
    writeFileSync(
      join(backstopInbox, '1714826790000-aaaaaa.md'),
      '---\nfrom: bob\n---\nbackstop must catch this\n'
    );
    await waitForBackstop(() => backstopReceived.length === 1);
    expect(backstopReceived[0]!.params.meta.messageFilename).toBe(
      '1714826790000-aaaaaa.md'
    );
    expect(backstopReceived[0]!.params.content).toBe(
      'backstop must catch this\n'
    );
  });

  it('backstop-only: multiple new files fire notifications in chronological order', async () => {
    writeFileSync(
      join(backstopInbox, '1714826790200-cccccc.md'),
      '---\nfrom: bob\n---\nthree\n'
    );
    writeFileSync(
      join(backstopInbox, '1714826790100-bbbbbb.md'),
      '---\nfrom: bob\n---\ntwo\n'
    );
    writeFileSync(
      join(backstopInbox, '1714826790000-aaaaaa.md'),
      '---\nfrom: bob\n---\none\n'
    );
    await waitForBackstop(() => backstopReceived.length === 3);
    const fns = backstopReceived.map((n) => n.params.meta.messageFilename);
    expect(fns).toEqual([
      '1714826790000-aaaaaa.md',
      '1714826790100-bbbbbb.md',
      '1714826790200-cccccc.md',
    ]);
  });

  it('at-least-once: files present at startup ARE delivered on watcher start (P5R-F2 fix)', async () => {
    // Simulate a fresh restart on an inbox with pending files. The
    // prior policy suppressed these on startup (delegating backlog
    // recovery to the boot ritual's `st_msg_ls`); a live repro
    // during the P5 team-standup re-run showed that policy loses
    // messages when Claude Code reconnects the stdio transport
    // mid-session (the fresh MCP process seeded them into `seen`
    // and the polling backstop couldn't rescue). Nathan chose
    // at-least-once semantics — duplicates on restart are the
    // accepted tradeoff, the alternative is losing messages the
    // agent may never wake to process. See CHANGELOG entry titled
    // "channel-watcher re-delivers inbox contents on start
    // (at-least-once)".
    await backstopHandle.close();
    // Plant three deliverable files BEFORE the new watcher starts —
    // filename order deliberately swapped (chronological c < b < a
    // by <unix-ms> prefix) to prove the initial scan sorts before
    // enqueueing.
    writeFileSync(
      join(backstopInbox, '1714826780200-cccccc.md'),
      '---\nfrom: bob\n---\nthird historically\n'
    );
    writeFileSync(
      join(backstopInbox, '1714826780100-bbbbbb.md'),
      '---\nfrom: bob\n---\nsecond historically\n'
    );
    writeFileSync(
      join(backstopInbox, '1714826780000-aaaaaa.md'),
      '---\nfrom: bob\n---\nfirst historically\n'
    );
    backstopHandle = createMcpServer({
      root: backstopRoot,
      identity: asIdentity('alice'),
      channel: true,
      channelWatcherOptions: {
        chokidarEnabled: false,
        pollBackstopIntervalMs: 50,
      },
    });
    backstopReceived = [];
    backstopClient.fallbackNotificationHandler = async (n) => {
      if (n.method === 'notifications/claude/channel') {
        backstopReceived.push(n as unknown as ChannelNotification);
      }
    };
    const [c, s] = InMemoryTransport.createLinkedPair();
    backstopClient = new Client({ name: 'test-cwatch-bs2', version: '1.0' });
    backstopClient.fallbackNotificationHandler = async (n) => {
      if (n.method === 'notifications/claude/channel') {
        backstopReceived.push(n as unknown as ChannelNotification);
      }
    };
    await Promise.all([
      backstopClient.connect(c),
      backstopHandle.mcp.connect(s),
    ]);
    await backstopHandle.startChannelWatcher();
    // All three should arrive in chronological order.
    await waitForBackstop(() => backstopReceived.length === 3);
    const initial = backstopReceived.map(
      (n) => n.params.meta.messageFilename
    );
    expect(initial).toEqual([
      '1714826780000-aaaaaa.md',
      '1714826780100-bbbbbb.md',
      '1714826780200-cccccc.md',
    ]);
    // Backstop tick must NOT re-emit any of the initial files —
    // exactly-once WITHIN this process. Wait a few ticks to prove it.
    await new Promise((r) => setTimeout(r, 250));
    expect(backstopReceived).toHaveLength(3);
    // A NEW file arriving after startup fires exactly once, as before.
    writeFileSync(
      join(backstopInbox, '1714826791000-newnew.md'),
      '---\nfrom: bob\n---\nfresh arrival\n'
    );
    await waitForBackstop(() => backstopReceived.length === 4);
    expect(backstopReceived[3]!.params.meta.messageFilename).toBe(
      '1714826791000-newnew.md'
    );
  });

  it('backstop-only: non-.md files (README, notes.txt) are ignored; outside .md fires', async () => {
    // Task #128: off-format `.md` files fire as outside messages;
    // non-.md paths (dotfiles, README, .txt) stay ignored.
    writeFileSync(join(backstopInbox, 'notes.txt'), 'ignore me\n');
    writeFileSync(join(backstopInbox, '.hidden.md'), 'ignore me too\n');
    writeFileSync(join(backstopInbox, 'random.md'), '---\nfrom: bob\n---\n');
    await waitForBackstop(() => backstopReceived.length === 1);
    expect(backstopReceived[0]!.params.meta.from).toBe('outside');
    expect(backstopReceived[0]!.params.meta.messageFilename).toBe('random.md');
  });

  it('close() disposes the backstop timer', async () => {
    await backstopHandle.close();
    writeFileSync(
      join(backstopInbox, '1714826790500-postclose.md'),
      '---\nfrom: bob\n---\nafter close\n'
    );
    await new Promise((r) => setTimeout(r, 200));
    expect(backstopReceived).toHaveLength(0);
    // Idempotent.
    await expect(backstopHandle.close()).resolves.not.toThrow();
  });
});

describe('channel-watcher — chokidar + backstop dedup (brief-020 HB-4)', () => {
  // With BOTH chokidar (in test-polling mode) and the backstop running
  // at aggressive intervals, a single arriving file must still fire
  // exactly one notification — the two paths share a `seen` set that
  // dedupes them.
  let dedupScratch: string;
  let dedupRoot: string;
  let dedupInbox: string;
  let dedupHandle: ReturnType<typeof createMcpServer>;
  let dedupClient: Client;
  let dedupReceived: ChannelNotification[];

  beforeEach(async () => {
    dedupScratch = mkdtempSync(join(tmpdir(), 'st-mcp-cwatch-dd-'));
    dedupRoot = join(dedupScratch, 'smalltalk');
    for (const id of ['alice', 'bob']) {
      mkdirSync(join(dedupRoot, id, 'inbox'), { recursive: true });
      mkdirSync(join(dedupRoot, id, 'archive'), { recursive: true });
    }
    dedupInbox = join(dedupRoot, 'alice', 'inbox');
    dedupHandle = createMcpServer({
      root: dedupRoot,
      identity: asIdentity('alice'),
      channel: true,
      channelWatcherOptions: {
        usePolling: true,
        pollInterval: 20,
        // Backstop at a fast enough tick to run alongside the
        // chokidar poll and race for the same file.
        pollBackstopIntervalMs: 30,
      },
    });
    dedupClient = new Client({ name: 'test-cwatch-dd', version: '1.0' });
    dedupReceived = [];
    dedupClient.fallbackNotificationHandler = async (n) => {
      if (n.method === 'notifications/claude/channel') {
        dedupReceived.push(n as unknown as ChannelNotification);
      }
    };
    const [c, s] = InMemoryTransport.createLinkedPair();
    await Promise.all([
      dedupClient.connect(c),
      dedupHandle.mcp.connect(s),
    ]);
    await dedupHandle.startChannelWatcher();
  });
  afterEach(async () => {
    await dedupHandle.close();
    rmSync(dedupScratch, { recursive: true, force: true });
  });

  it('a single file fires exactly one notification even with both paths racing', async () => {
    writeFileSync(
      join(dedupInbox, '1714826790000-aaaaaa.md'),
      '---\nfrom: bob\n---\nno double-emit\n'
    );
    // Wait long enough that either path could have fired multiple
    // ticks (~200ms >> 30ms backstop + 20ms chokidar poll).
    await new Promise((r) => setTimeout(r, 250));
    expect(dedupReceived).toHaveLength(1);
    expect(dedupReceived[0]!.params.meta.messageFilename).toBe(
      '1714826790000-aaaaaa.md'
    );
  });
});

// ─── Lifecycle: runWith resolves on transport close ───────────────────
//
// Standalone describe block so it doesn't share the global `handle` /
// `client` / etc. — `runWith` owns its own transport and we close the
// pair ourselves.

describe('channel-watcher — runWith lifecycle', () => {
  let scratch2: string;
  let stRoot2: string;

  beforeEach(() => {
    scratch2 = mkdtempSync(join(tmpdir(), 'st-mcp-rw-'));
    stRoot2 = join(scratch2, 'smalltalk');
    for (const id of ['alice', 'bob']) {
      mkdirSync(join(stRoot2, id, 'inbox'), { recursive: true });
      mkdirSync(join(stRoot2, id, 'archive'), { recursive: true });
    }
  });
  afterEach(() => {
    rmSync(scratch2, { recursive: true, force: true });
  });

  it('host closing the transport → runWith resolves and the watcher is torn down', async () => {
    const h = createMcpServer({
      root: stRoot2,
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
      join(stRoot2, 'alice', 'inbox', '1714826789010-aaaaaa.md'),
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
      join(stRoot2, 'alice', 'inbox', '1714826789020-bbbbbb.md'),
      '---\nfrom: bob\n---\nafter close\n'
    );
    await new Promise((r) => setTimeout(r, 200));
    expect(seen).toHaveLength(1);
  });
});
