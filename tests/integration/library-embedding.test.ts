// tests/integration/library-embedding.test.ts — proves createSt works
// the way an Electron main process or a Node TUI would use it.
//
// Two Smalltalk handles bound to two distinct $ST_ROOT directories ("two
// machines"). Real rsync between them via the local: peer spec. Real
// filesystem. The whole loop a real embedder would drive.

import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  asIdentity,
  type St,
  createSt,
  IdentityNotHostedError,
} from '../../src/index.ts';
import {
  cleanupRoot,
  mkIdentity,
  mkRoot,
  rsyncAvailable,
} from './helpers.ts';

const skip = !rsyncAvailable();
const d = skip ? describe.skip : describe;

d('library embedding (createSt)', () => {
  let rootA: string;
  let rootB: string;
  let stA: St;
  let stB: St;

  beforeEach(() => {
    rootA = mkRoot();
    rootB = mkRoot();
    mkIdentity(rootA, 'alice');
    mkIdentity(rootB, 'bob');
    stA = createSt({ root: rootA, identity: asIdentity('alice') });
    stB = createSt({ root: rootB, identity: asIdentity('bob') });
  });
  afterEach(() => {
    cleanupRoot(rootA);
    cleanupRoot(rootB);
  });

  // ── send + sync.push + ls ────────────────────────────────────────

  it('alice.send → A.sync.push(local:B) → bob.ls returns the message', async () => {
    const filename = await stA.send(asIdentity('bob'), 'hello bob');
    expect(filename).toMatch(/^[0-9]{13}-[0-9a-z]{6}\.md$/);
    // On A, alice's view of bob/inbox already has it.
    expect(existsSync(join(rootA, 'bob', 'inbox', filename))).toBe(true);

    const pushResult = await stA.sync.push(`local:${rootB}`);
    // SyncResult is { stdout, stderr } — the library captures rsync's
    // banner output but never writes to process stdio.
    expect(typeof pushResult.stdout).toBe('string');
    expect(typeof pushResult.stderr).toBe('string');

    // On B, bob's inbox now has the file.
    const incoming = await stB.ls();
    expect(incoming).toEqual([filename]);
    // And on A, alice's view persists (rsync -a is copy-not-move).
    expect(existsSync(join(rootA, 'bob', 'inbox', filename))).toBe(true);
  });

  // ── read + archive ──────────────────────────────────────────────

  it('bob.read returns a typed Message; bob.archive moves it', async () => {
    const filename = await stA.send(asIdentity('bob'), 'body', {
      subject: 'hi',
    });
    await stA.sync.push(`local:${rootB}`);

    const m = await stB.read(asIdentity('bob'), filename);
    expect(m.identity).toBe('bob');
    expect(m.filename).toBe(filename);
    expect(m.folder).toBe('inbox');
    expect(m.message.from).toBe('alice');
    expect(m.message.subject).toBe('hi');
    expect(m.message.body).toBe('body\n');

    await stB.archive(asIdentity('bob'), filename);
    expect(await stB.ls()).toEqual([]);
    expect(await stB.ls(asIdentity('bob'), { archive: true })).toEqual([
      filename,
    ]);
  });

  // ── status round-trip ────────────────────────────────────────────

  it('alice.setStatus + sync → bob.getStatus(alice) sees the new state', async () => {
    expect(await stA.getStatus(asIdentity('alice'))).toBe('offline');
    await stA.setStatus(asIdentity('alice'), 'busy');
    expect(await stA.getStatus(asIdentity('alice'))).toBe('busy');
    await stA.sync.push(`local:${rootB}`);

    expect(await stB.getStatus(asIdentity('alice'))).toBe('busy');
    // bob's own status defaults to offline (no file yet on B).
    expect(await stB.getStatus(asIdentity('bob'))).toBe('offline');
  });

  // ── watch + AbortController ──────────────────────────────────────

  it('stB.watch yields newly-arrived files; AbortController stops it', async () => {
    const filename1 = await stA.send(asIdentity('bob'), 'first');
    await stA.sync.push(`local:${rootB}`);

    const ac = new AbortController();
    const seen: string[] = [];
    const consume = (async () => {
      for await (const ev of stB.watch(asIdentity('bob'), {
        intervalMs: 50,
        signal: ac.signal,
      })) {
        seen.push(ev.filename);
        if (seen.length === 2) ac.abort();
      }
    })();

    // Drop a SECOND file via a real sync round so the live poll picks
    // it up after replay drains the first one.
    await stA.send(asIdentity('bob'), 'second');
    await stA.sync.push(`local:${rootB}`);

    await consume;
    expect(seen).toContain(filename1);
    expect(seen).toHaveLength(2);
  });

  // ── thread (cross-identity walk) ────────────────────────────────

  it('thread walks ancestors + descendants and returns MessageWithLocation[]', async () => {
    // alice → bob with a known seed.
    const f1 = await stA.send(asIdentity('bob'), 'root', {
      subject: 'A',
    });
    // Force the second send into a strictly-later ms so the walker's
    // chronological sort matches the send order (otherwise two sends
    // sharing a ms break the [f1, f2] ordering deterministically).
    await new Promise((r) => setTimeout(r, 2));
    const f2 = await stA.send(asIdentity('bob'), 'reply', {
      subject: 'B',
      inReplyTo: f1,
    });
    await stA.sync.push(`local:${rootB}`);

    const thread = await stB.thread(asIdentity('bob'), f1);
    expect(thread.map((m) => m.filename)).toEqual([f1, f2]);
    expect(thread[0]?.message.subject).toBe('A');
    expect(thread[1]?.message.subject).toBe('B');
  });

  // ── typed errors are catchable across the import boundary ───────

  it('typed errors surface through the API for embedder pattern-matching', async () => {
    let caught: unknown;
    try {
      await stB.archive(
        asIdentity('ghost'),
        // The branding cast is intentional: we want to exercise the
        // identity-folder-missing path, not the filename-grammar path.
        '1714826789010-aaaaaa.md' as never
      );
    } catch (e) {
      caught = e;
    }
    expect(caught).toBeInstanceOf(IdentityNotHostedError);
    if (caught instanceof IdentityNotHostedError) {
      expect(caught.identity).toBe('ghost');
      expect(caught.code).toBe('IDENTITY_NOT_HOSTED');
    }
  });

  // ── stdin/stdout isolation ──────────────────────────────────────

  it('the library never writes to process.stdout / process.stderr', async () => {
    // Capture writes by stubbing the prototypes for the call.
    const origStdoutWrite = process.stdout.write.bind(process.stdout);
    const origStderrWrite = process.stderr.write.bind(process.stderr);
    let stdoutCalls = 0;
    let stderrCalls = 0;
    process.stdout.write = ((chunk: unknown, ...rest: unknown[]) => {
      stdoutCalls += 1;
      void chunk;
      void rest;
      return true;
    }) as typeof process.stdout.write;
    process.stderr.write = ((chunk: unknown, ...rest: unknown[]) => {
      stderrCalls += 1;
      void chunk;
      void rest;
      return true;
    }) as typeof process.stderr.write;

    try {
      await stA.send(asIdentity('bob'), 'silent');
      await stA.sync.push(`local:${rootB}`);
      await stB.ls();
      await stB.getStatus(asIdentity('alice'));
    } finally {
      process.stdout.write = origStdoutWrite;
      process.stderr.write = origStderrWrite;
    }

    expect(stdoutCalls).toBe(0);
    expect(stderrCalls).toBe(0);
  });
});
