// tests/integration/sync-errors.test.ts — failure paths through `st sync`.
//
// The cases pin: rsync transport failure (bogus host), rsync target failure
// (unwritable local dir), missing peers.yaml, empty peers.yaml, and the
// guarantee that the universal pre-command sweep runs even when a sync's
// rsync stage fails (Z1).

import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { afterAll, beforeEach, describe, expect, it } from 'vitest';

import {
  cleanupRoot,
  listArchive,
  listInbox,
  mkIdentity,
  mkRoot,
  mkScratch,
  rsyncAvailable,
  runSt,
} from './helpers.ts';

const skip = !rsyncAvailable();
const d = skip ? describe.skip : describe;

d('sync error paths', () => {
  let root: string;
  let stConfig: string;
  const allRoots: string[] = [];

  beforeEach(() => {
    root = mkRoot();
    stConfig = mkScratch();
    mkIdentity(root, 'alice');
    allRoots.push(root, stConfig);
  });

  afterAll(() => {
    for (const p of allRoots) cleanupRoot(p);
  });

  // ── rsync transport failure ────────────────────────────────────────

  it('push to a bogus ssh host: non-zero exit, "rsync push failed" on stderr', () => {
    // Use a non-resolvable hostname so rsync fails fast at the ssh layer.
    const r = runSt(
      ['sync', 'push', 'this-host-does-not-resolve.invalid.example.com'],
      {
        stRoot: root,
        stIdentity: 'alice',
        timeoutMs: 20_000,
      }
    );
    expect(r.exitCode).not.toBe(0);
    expect(r.stderr).toContain('rsync push failed');
  });

  it('rsync failure leaves the local tree unchanged', () => {
    // Pre-populate alice's outbound with a message.
    runSt(['message', 'send', 'bob', '--from', 'alice'], {
      stRoot: root,
      stIdentity: 'alice',
      stdin: 'preserved',
    });
    const before = listInbox(root, 'bob');

    runSt(
      ['sync', 'push', 'this-host-does-not-resolve.invalid.example.com'],
      {
        stRoot: root,
        stIdentity: 'alice',
        timeoutMs: 20_000,
      }
    );

    // Local tree intact.
    expect(listInbox(root, 'bob')).toEqual(before);
  });

  // ── rsync target failure (local) ───────────────────────────────────

  it('push to local:<unwritable target> propagates the failure', () => {
    // Place the target under a parent we mkdir as 0500 (read+execute, no
    // write). The local: resolver's mkdir-recursive call fails with
    // EACCES before rsync even runs — but the failure still propagates
    // as a non-zero exit with a "smalltalk:" prefix on stderr, which is the
    // documented contract.
    const lockedParent = join(mkScratch(), 'locked');
    mkdirSync(lockedParent, { recursive: true, mode: 0o500 });
    const target = join(lockedParent, 'cannot-create');

    const r = runSt(['sync', 'push', `local:${target}`], {
      stRoot: root,
      stIdentity: 'alice',
    });
    expect(r.exitCode).not.toBe(0);
    expect(r.stderr).toMatch(/st:.*(permission denied|rsync push failed)/);
  });

  // ── peers.yaml missing / empty ─────────────────────────────────────

  it('sync --all with no peers.yaml exits non-zero with a clear message', () => {
    const r = runSt(['sync', '--all'], {
      stRoot: root,
      stConfig,
      stIdentity: 'alice',
    });
    expect(r.exitCode).not.toBe(0);
    expect(r.stderr).toContain('no peers configured');
  });

  it('sync --all with empty peers.yaml (no entries) exits non-zero', () => {
    writeFileSync(join(stConfig, 'peers.yaml'), '# only comments\n');
    const r = runSt(['sync', '--all'], {
      stRoot: root,
      stConfig,
      stIdentity: 'alice',
    });
    expect(r.exitCode).not.toBe(0);
    expect(r.stderr).toContain('no peers found');
  });

  it('sync push --all with no peers.yaml errors loudly', () => {
    const r = runSt(['sync', 'push', '--all'], {
      stRoot: root,
      stConfig,
      stIdentity: 'alice',
    });
    expect(r.exitCode).not.toBe(0);
    expect(r.stderr).toContain('no peers configured');
  });

  it('sync pull --all with no peers.yaml errors loudly', () => {
    const r = runSt(['sync', 'pull', '--all'], {
      stRoot: root,
      stConfig,
      stIdentity: 'alice',
    });
    expect(r.exitCode).not.toBe(0);
    expect(r.stderr).toContain('no peers configured');
  });

  // ── pre-command sweep still runs even when sync fails ──────────────

  it('universal pre-command sweep still cleans local zombies even when rsync fails', () => {
    // Create a zombie state on alice's machine: identical inbox + archive.
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(join(root, 'alice', 'inbox', f), 'same');
    writeFileSync(join(root, 'alice', 'archive', f), 'same');

    // Trigger a sync that will fail at the rsync stage. The dispatcher's
    // pre-command sweep runs before dispatching to `sync`, so the zombie
    // is cleaned regardless of rsync's exit code.
    const r = runSt(
      ['sync', 'push', 'this-host-does-not-resolve.invalid.example.com'],
      {
        stRoot: root,
        stIdentity: 'alice',
        timeoutMs: 20_000,
      }
    );
    expect(r.exitCode).not.toBe(0);

    // The zombie inbox copy is gone; archive copy remains.
    expect(existsSync(join(root, 'alice', 'inbox', f))).toBe(false);
    expect(existsSync(join(root, 'alice', 'archive', f))).toBe(true);
    expect(listInbox(root, 'alice')).toEqual([]);
    expect(listArchive(root, 'alice')).toEqual([f]);
  });

  // ── --all sweep guard ──────────────────────────────────────────────

  it('sync --all with verb "sweep" errors: sweep is local-only', () => {
    writeFileSync(
      join(stConfig, 'peers.yaml'),
      `bobby: local:${join(mkScratch(), 'b')}\n`
    );
    const r = runSt(['sync', 'sweep', '--all'], {
      stRoot: root,
      stConfig,
      stIdentity: 'alice',
    });
    expect(r.exitCode).not.toBe(0);
    expect(r.stderr).toContain('sweep is local-only');
  });
});
