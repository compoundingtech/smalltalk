// tests/integration/walkthrough.test.ts — Acts I–VIII from notes/walkthrough.md.
//
// Three identities (alice/bob/operator) on three roots, real rsync between
// them via `local:<path>` peers, final state matches the walkthrough's
// "every machine has the same five-message thread" acceptance.

import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';

import {
  cleanupRoot,
  listArchive,
  listInbox,
  mkIdentity,
  mkRoot,
  rsyncAvailable,
  runSt,
} from './helpers.ts';

const skip = !rsyncAvailable();
const d = skip ? describe.skip : describe;

d('walkthrough — Acts I–VIII', () => {
  let A: string; // alice's machine
  let B: string; // bob's machine
  let C: string; // operator's laptop

  beforeAll(() => {
    A = mkRoot();
    B = mkRoot();
    C = mkRoot();
    mkIdentity(A, 'alice');
    mkIdentity(B, 'bob');
    mkIdentity(C, 'operator');
  });

  afterAll(() => {
    cleanupRoot(A);
    cleanupRoot(B);
    cleanupRoot(C);
  });

  // Pairwise push between two roots in both directions, with a sweep on
  // each side. After this round, every file under either root exists on
  // both — used to fan messages out across the three-machine grid.
  function bidirectional(rootX: string, idX: string, rootY: string, idY: string): void {
    runSt(['sync', 'push', `local:${rootY}`], { stRoot: rootX, stIdentity: idX });
    runSt(['sync', 'pull', `local:${rootY}`], { stRoot: rootX, stIdentity: idX });
    runSt(['sync', 'push', `local:${rootX}`], { stRoot: rootY, stIdentity: idY });
    runSt(['sync', 'pull', `local:${rootX}`], { stRoot: rootY, stIdentity: idY });
  }

  // Full round-robin: each pair syncs bidirectionally.
  function syncEveryone(): void {
    bidirectional(A, 'alice', B, 'bob');
    bidirectional(B, 'bob', C, 'operator');
    bidirectional(A, 'alice', C, 'operator');
  }

  it('plays through all eight acts and converges to a five-message thread in archive/ on every machine', () => {
    // ── Act II — alice sends operator a question ─────────────────────────
    const m1 = runSt(
      [
        'message',
        'send',
        'operator',
        '--from',
        'alice',
        '--subject',
        'auth middleware: drop legacy session cookie?',
      ],
      {
        stRoot: A,
        stIdentity: 'alice',
        stdin:
          'The new auth path replaces the old one cleanly. The legacy session cookie\nis now dead code — should I remove it, or keep it as a compat shim until\nthe next release?\n',
      }
    );
    expect(m1.exitCode).toBe(0);
    const f1 = m1.stdout.trim();
    expect(f1).toMatch(/^[0-9]{13}-[0-9a-z]{6}\.md$/);
    expect(existsSync(join(A, 'operator', 'inbox', f1))).toBe(true);

    // ── Act III — sync delivers; operator reads, replies, archives ───────
    syncEveryone();
    expect(existsSync(join(C, 'operator', 'inbox', f1))).toBe(true);

    const m2 = runSt(
      [
        'message',
        'send',
        'alice',
        '--from',
        'operator',
        '--subject',
        're: auth middleware: drop legacy session cookie?',
        '--in-reply-to',
        f1,
      ],
      {
        stRoot: C,
        stIdentity: 'operator',
        stdin:
          'drop it. nothing depends on it now and the compat shim adds complexity for no users.\n',
      }
    );
    expect(m2.exitCode).toBe(0);
    const f2 = m2.stdout.trim();

    const arch1 = runSt(['message', 'archive', 'operator', f1], {
      stRoot: C,
      stIdentity: 'operator',
    });
    expect(arch1.exitCode).toBe(0);
    expect(listArchive(C, 'operator')).toEqual([f1]);

    // ── Act IV — alice fans out to bob ──────────────────────────────────
    syncEveryone();
    // Alice has operator's reply locally now.
    expect(existsSync(join(A, 'alice', 'inbox', f2))).toBe(true);

    // Note: the walkthrough's prose has m3 logically following m2 (alice
    // forwards the decision to bob) — the example shell command in the
    // walkthrough doesn't show --in-reply-to, but to make Act VIII's
    // 5-message thread output true the chain has to be wired up. We thread
    // m3 onto m2 here.
    const m3 = runSt(
      [
        'message',
        'send',
        'bob',
        '--from',
        'alice',
        '--subject',
        'FYI: dropping legacy session cookie path',
        '--tags',
        'auth,orchestration',
        '--in-reply-to',
        f2,
      ],
      {
        stRoot: A,
        stIdentity: 'alice',
        stdin:
          "operator greenlit dropping the legacy session cookie. you'll want to drop the matching server-side verifier so we don't ship a no-op middleware.\n",
      }
    );
    expect(m3.exitCode).toBe(0);
    const f3 = m3.stdout.trim();

    const arch2 = runSt(['message', 'archive', 'alice', f2], {
      stRoot: A,
      stIdentity: 'alice',
    });
    expect(arch2.exitCode).toBe(0);

    // ── Act V — bob is busy ────────────────────────────────────────────
    const setStatus = runSt(['status', '--set', 'busy'], {
      stRoot: B,
      stIdentity: 'bob',
    });
    expect(setStatus.exitCode).toBe(0);
    expect(setStatus.stdout.trim()).toBe('status: busy');

    syncEveryone();
    // Alice + operator see bob's status as busy via their synced view.
    const aliceSeesBob = runSt(['status', 'bob'], {
      stRoot: A,
      stIdentity: 'alice',
    });
    expect(aliceSeesBob.stdout.trim()).toBe('busy');
    const operatorSeesBob = runSt(['status', 'bob'], {
      stRoot: C,
      stIdentity: 'operator',
    });
    expect(operatorSeesBob.stdout.trim()).toBe('busy');

    // ── Act VI — bob comes available, replies, CCs operator, archives ────
    runSt(['status', '--set', 'available'], {
      stRoot: B,
      stIdentity: 'bob',
    });

    const m4 = runSt(
      [
        'message',
        'send',
        'alice',
        '--from',
        'bob',
        '--subject',
        're: dropping legacy session cookie path',
        '--in-reply-to',
        f3,
      ],
      {
        stRoot: B,
        stIdentity: 'bob',
        stdin:
          'done. server-side session-token verifier removed in commit a7f3c21. all auth tests still green.\n',
      }
    );
    expect(m4.exitCode).toBe(0);
    const f4 = m4.stdout.trim();

    // Same note as m3: m5 is a CC about the same topic, threaded onto m3
    // so Act VIII's flat thread output reaches it from the original f1
    // seed. Walkthrough prose says "FYI alice and I" — that's a
    // continuation of the m3 chain.
    const m5 = runSt(
      [
        'message',
        'send',
        'operator',
        '--from',
        'bob',
        '--subject',
        'auth cleanup landed',
        '--tags',
        'auth',
        '--in-reply-to',
        f3,
      ],
      {
        stRoot: B,
        stIdentity: 'bob',
        stdin:
          'FYI alice and I dropped the legacy session-cookie path on both ends. nothing breaks; tests green.\n',
      }
    );
    expect(m5.exitCode).toBe(0);
    const f5 = m5.stdout.trim();

    const arch3 = runSt(['message', 'archive', 'bob', f3], {
      stRoot: B,
      stIdentity: 'bob',
    });
    expect(arch3.exitCode).toBe(0);

    // ── Act VII — sync rolls out, sweep converges everyone ─────────────
    syncEveryone();
    syncEveryone(); // second pass to drain cross-machine zombies

    // Alice archives bob's reply (m4) on her machine.
    runSt(['message', 'archive', 'alice', f4], {
      stRoot: A,
      stIdentity: 'alice',
    });
    // Myobie archives bob's CC (m5) on his laptop.
    runSt(['message', 'archive', 'operator', f5], {
      stRoot: C,
      stIdentity: 'operator',
    });
    syncEveryone();
    syncEveryone();

    // ── Final state check ──────────────────────────────────────────────
    // Every machine has the same final shape:
    //   alice/archive   = [f2, f4]
    //   bob/archive     = [f3]
    //   operator/archive  = [f1, f5]
    // and every inbox is empty.
    const finalArchives = {
      alice: [f2, f4].sort(),
      bob: [f3].sort(),
      operator: [f1, f5].sort(),
    };
    for (const root of [A, B, C]) {
      expect(listInbox(root, 'alice')).toEqual([]);
      expect(listInbox(root, 'bob')).toEqual([]);
      expect(listInbox(root, 'operator')).toEqual([]);
      expect(listArchive(root, 'alice')).toEqual(finalArchives.alice);
      expect(listArchive(root, 'bob')).toEqual(finalArchives.bob);
      expect(listArchive(root, 'operator')).toEqual(finalArchives.operator);
    }

    // ── Act VIII — operator pulls the whole thread ───────────────────────
    // Seeded from the original question; flat default = chronological.
    const thread = runSt(['message', 'thread', 'operator', f1], {
      stRoot: C,
      stIdentity: 'operator',
    });
    expect(thread.exitCode).toBe(0);
    const lines = thread.stdout.trim().split('\n');
    // Five messages, in chronological order (filename ascending).
    const filenamesInOrder = lines.map((l) => l.split('\t')[0]);
    expect(filenamesInOrder).toEqual([f1, f2, f3, f4, f5].sort());
    // Each line shape: "<filename>\t<from>\t<subject>".
    expect(lines[0]).toBe(
      `${f1}\talice\tauth middleware: drop legacy session cookie?`
    );
    expect(lines[1]).toBe(
      `${f2}\toperator\tre: auth middleware: drop legacy session cookie?`
    );
    expect(lines[2]).toBe(
      `${f3}\talice\tFYI: dropping legacy session cookie path`
    );
    expect(lines[3]).toBe(
      `${f4}\tbob\tre: dropping legacy session cookie path`
    );
    expect(lines[4]).toBe(`${f5}\tbob\tauth cleanup landed`);

    // --tree mode shows the same five messages, depth-indented.
    const tree = runSt(['message', 'thread', 'operator', f1, '--tree'], {
      stRoot: C,
      stIdentity: 'operator',
    });
    expect(tree.exitCode).toBe(0);
    expect(tree.stdout).toContain(f1); // root
    expect(tree.stdout).toContain(`  ${f2}`); // depth 1 (operator's reply)
    expect(tree.stdout).toContain(`  ${f4}`); // depth 1 (bob's reply to f3)
  });
});
