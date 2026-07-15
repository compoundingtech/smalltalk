// tests/unit/sync-fabric.test.ts — the `st sync fabric` run side and, above
// all, the SAFE REMOTE SWEEP: the guard that deletes only twin-verified
// tombstones from the remote inbox and never a fresh, unreconciled delivery.

import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { sweepPlan } from '../../src/common.ts';
import type { RsyncResult } from '../../src/commands/sync.ts';
import {
  assertModernRsync,
  assertSocketPathOk,
  fabricExposeArgs,
  fabricSyncCycle,
  MAX_UNIX_SOCKET_PATH,
  remoteSweepFilters,
  rshScriptContent,
  rsyncdConfContent,
} from '../../src/commands/sync-fabric.ts';

let root: string;

beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), 'st-fabric-test-'));
});
afterEach(() => {
  rmSync(root, { recursive: true, force: true });
});

// LAYOUT-004 filenames: 13-digit ms + '-' + 6 lowercase alnum + '.md'.
const TWIN = '1714826789010-aaaaaa.md';
const FRESH = '1714826789020-bbbbbb.md';
const OTHER = '1714826789030-cccccc.md';

function mkId(id: string): void {
  mkdirSync(join(root, id, 'inbox'), { recursive: true });
  mkdirSync(join(root, id, 'archive'), { recursive: true });
}
function put(id: string, folder: 'inbox' | 'archive', name: string, body: string): void {
  writeFileSync(join(root, id, folder, name), body);
}
function inbox(id: string): string[] {
  const d = join(root, id, 'inbox');
  return existsSync(d) ? readdirSync(d).sort() : [];
}

// ─── remoteSweepFilters (the deletion-safety filter builder) ─────────────

describe('remoteSweepFilters', () => {
  it('empty plan → no filters (caller skips the remote-sweep rsync)', () => {
    expect(remoteSweepFilters([])).toEqual([]);
  });

  it('risks each named path, then protects everything else — order matters', () => {
    const out = remoteSweepFilters(['bob/inbox/X.md', 'ann/inbox/Y.md']);
    expect(out).toEqual([
      '--delete',
      '--filter=R bob/inbox/X.md',
      '--filter=R ann/inbox/Y.md',
      '--filter=P **',
    ]);
    // The protect-all catch-all MUST come last (rsync is first-match-wins).
    expect(out[out.length - 1]).toBe('--filter=P **');
    // Every specific risk rule precedes it.
    const protectIdx = out.indexOf('--filter=P **');
    for (const rel of ['bob/inbox/X.md', 'ann/inbox/Y.md']) {
      expect(out.indexOf(`--filter=R ${rel}`)).toBeLessThan(protectIdx);
    }
  });
});

// ─── sweepPlan discrimination = the twinned-check under test ─────────────

describe('sweepPlan (the twin-verified delete-set)', () => {
  it('POSITIVE: a byte-identical inbox/archive twin is in the plan', () => {
    mkId('bob');
    put('bob', 'inbox', TWIN, 'same');
    put('bob', 'archive', TWIN, 'same');
    const rels = sweepPlan(root).map((e) => e.rel);
    expect(rels).toContain(`bob/inbox/${TWIN}`);
  });

  it('SAFETY: a fresh, untwinned delivery is NOT in the plan (never deleted)', () => {
    mkId('bob');
    // A genuine tombstone twin...
    put('bob', 'inbox', TWIN, 'same');
    put('bob', 'archive', TWIN, 'same');
    // ...alongside a fresh delivery with NO archive twin.
    put('bob', 'inbox', FRESH, 'unread message');
    const rels = sweepPlan(root).map((e) => e.rel);
    expect(rels).toContain(`bob/inbox/${TWIN}`); // twin swept
    expect(rels).not.toContain(`bob/inbox/${FRESH}`); // fresh survives
  });

  it('SAFETY: a divergent pair (inbox edited) is NOT in the plan', () => {
    mkId('bob');
    put('bob', 'inbox', OTHER, 'edited locally');
    put('bob', 'archive', OTHER, 'original archived');
    expect(sweepPlan(root).map((e) => e.rel)).not.toContain(`bob/inbox/${OTHER}`);
  });

  it('MUTATION-NET: a name-only check (dropping the byte + archive-existence guards) WOULD delete the fresh file — proving the guard discriminates', () => {
    mkId('bob');
    put('bob', 'inbox', TWIN, 'same');
    put('bob', 'archive', TWIN, 'same');
    put('bob', 'inbox', FRESH, 'unread message');

    // The real guard: byte-identical archive twin required.
    const real = sweepPlan(root).map((e) => e.rel).sort();

    // A mutant that keeps only "is a valid-looking inbox file" — i.e. drops the
    // `existsSync(archive) && inboxBuf.equals(archiveBuf)` twinned-check. This
    // is exactly the mutation cos asked the test to catch.
    const mutant = readdirSync(join(root, 'bob', 'inbox'))
      .map((n) => `bob/inbox/${n}`)
      .sort();

    // The real guard excludes the fresh delivery; the mutant includes it.
    expect(real).toEqual([`bob/inbox/${TWIN}`]);
    expect(mutant).toContain(`bob/inbox/${FRESH}`);
    expect(real).not.toEqual(mutant);
  });
});

// ─── fabricSyncCycle orchestration + end-to-end safety ───────────────────

interface MockState {
  calls: string[][];
  status: number;
}
function mockRsync(state: MockState): (args: string[]) => RsyncResult {
  return (args) => {
    state.calls.push(args);
    return { status: state.status };
  };
}

describe('fabricSyncCycle', () => {
  it('pull → local-sweep → remote-sweep(twin only) → push; fresh delivery survives', () => {
    mkId('bob');
    put('bob', 'inbox', TWIN, 'same');
    put('bob', 'archive', TWIN, 'same');
    put('bob', 'inbox', FRESH, 'unread message');

    const state: MockState = { calls: [], status: 0 };
    const remoteRoot = 'peer::smalltalk/';
    const r = fabricSyncCycle(root, remoteRoot, [], 'inbox-archive', {
      runRsync: mockRsync(state),
    });

    // Result: exactly the twin was remote-deleted; the fresh file was not.
    expect(r.localRemoved).toBe(1);
    expect(r.remoteDeleted).toEqual([`bob/inbox/${TWIN}`]);

    // Local: twin gone, fresh delivery preserved, archive tombstone kept.
    expect(inbox('bob')).toEqual([FRESH]);
    expect(existsSync(join(root, 'bob', 'archive', TWIN))).toBe(true);

    // Three rsync calls: pull, remote-sweep, push (in that order).
    expect(state.calls).toHaveLength(3);
    const [pull, remoteSweep, push] = state.calls;
    // pull: remote -> local
    expect(pull![pull!.length - 2]).toBe(remoteRoot);
    expect(pull![pull!.length - 1]).toBe(`${root}/`);
    // remote-sweep: risks ONLY the twin, protects all else, deletes.
    expect(remoteSweep).toContain('--delete');
    expect(remoteSweep).toContain(`--filter=R bob/inbox/${TWIN}`);
    expect(remoteSweep).toContain('--filter=P **');
    expect(remoteSweep).not.toContain(`--filter=R bob/inbox/${FRESH}`);
    // push: local -> remote, and NEVER --delete (union only).
    expect(push![push!.length - 2]).toBe(`${root}/`);
    expect(push![push!.length - 1]).toBe(remoteRoot);
    expect(push).not.toContain('--delete');
  });

  it('no twins → no remote-sweep rsync (only pull + push)', () => {
    mkId('bob');
    put('bob', 'inbox', FRESH, 'unread message');
    const state: MockState = { calls: [], status: 0 };
    const r = fabricSyncCycle(root, 'peer::smalltalk/', [], 'inbox-archive', {
      runRsync: mockRsync(state),
    });
    expect(r.remoteDeleted).toEqual([]);
    expect(state.calls).toHaveLength(2); // pull, push — no delete pass
    for (const c of state.calls) expect(c).not.toContain('--delete');
  });

  it('threads transport args (e.g. -e rsh) into every rsync call', () => {
    mkId('bob');
    put('bob', 'inbox', TWIN, 'same');
    put('bob', 'archive', TWIN, 'same');
    const state: MockState = { calls: [], status: 0 };
    fabricSyncCycle(root, 'peer::smalltalk/', ['-e', '/tmp/rsh.sh'], 'inbox-archive', {
      runRsync: mockRsync(state),
    });
    for (const c of state.calls) {
      expect(c.slice(0, 2)).toEqual(['-e', '/tmp/rsh.sh']);
    }
  });

  it('sets an rsync --timeout on every call (a dead tunnel fails fast, never wedges)', () => {
    mkId('bob');
    put('bob', 'inbox', TWIN, 'same');
    put('bob', 'archive', TWIN, 'same');
    const state: MockState = { calls: [], status: 0 };
    // default timeout
    fabricSyncCycle(root, 'peer::smalltalk/', [], 'inbox-archive', {
      runRsync: mockRsync(state),
    });
    for (const c of state.calls) {
      expect(c.some((a) => a.startsWith('--timeout='))).toBe(true);
    }
    // explicit override flows through, positioned after transport args
    const s2: MockState = { calls: [], status: 0 };
    fabricSyncCycle(root, 'peer::smalltalk/', ['-e', '/tmp/rsh.sh'], 'inbox-archive', {
      runRsync: mockRsync(s2),
    }, 30);
    for (const c of s2.calls) {
      expect(c).toContain('--timeout=30');
      expect(c.slice(0, 3)).toEqual(['-e', '/tmp/rsh.sh', '--timeout=30']);
    }
  });

  it('a failed pull throws and does not sweep locally', () => {
    mkId('bob');
    put('bob', 'inbox', TWIN, 'same');
    put('bob', 'archive', TWIN, 'same');
    const state: MockState = { calls: [], status: 23 };
    expect(() =>
      fabricSyncCycle(root, 'peer::smalltalk/', [], 'inbox-archive', {
        runRsync: mockRsync(state),
      })
    ).toThrowError(/fabric pull failed/);
    // Pull failed before any sweep — the twin is still on disk locally.
    expect(existsSync(join(root, 'bob', 'inbox', TWIN))).toBe(true);
  });

  it('scope=all sends no include/exclude filters', () => {
    mkId('bob');
    const state: MockState = { calls: [], status: 0 };
    fabricSyncCycle(root, 'peer::smalltalk/', [], 'all', {
      runRsync: mockRsync(state),
    });
    for (const c of state.calls) {
      expect(c.some((a) => a.startsWith('--include='))).toBe(false);
      expect(c.some((a) => a.startsWith('--exclude='))).toBe(false);
    }
  });
});

// ─── transport hardening (decision b) ────────────────────────────────────

describe('transport hardening', () => {
  it('assertModernRsync rejects openrsync with an actionable error', () => {
    expect(() => assertModernRsync('openrsync: protocol version 29')).toThrowError(
      /openrsync/
    );
    expect(() => assertModernRsync('openrsync: protocol version 29')).toThrowError(
      /brew install rsync/
    );
  });

  it('assertModernRsync passes GNU rsync', () => {
    expect(() =>
      assertModernRsync('rsync  version 3.4.4  protocol version 32')
    ).not.toThrow();
  });

  it('assertSocketPathOk throws past the unix-socket length limit', () => {
    const okPath = '/tmp/' + 'a'.repeat(MAX_UNIX_SOCKET_PATH - 6);
    expect(okPath.length).toBeLessThanOrEqual(MAX_UNIX_SOCKET_PATH);
    expect(() => assertSocketPathOk(okPath)).not.toThrow();
    const tooLong = '/tmp/' + 'a'.repeat(MAX_UNIX_SOCKET_PATH);
    expect(() => assertSocketPathOk(tooLong)).toThrowError(/unix-socket limit/);
  });

  it('rshScriptContent bridges the given socket and ignores rsync-appended args', () => {
    const s = rshScriptContent('/tmp/dial.sock');
    expect(s.startsWith('#!/bin/sh\n')).toBe(true);
    expect(s).toContain('exec socat - UNIX-CONNECT:/tmp/dial.sock');
  });
});

// ─── serve side (fabric exec-expose registration) ────────────────────────

describe('serve-side config + registration', () => {
  it('rsyncdConfContent declares a read-write module pointing at the root', () => {
    const conf = rsyncdConfContent('/home/u/.local/state/convoy');
    expect(conf).toContain('[smalltalk]');
    expect(conf).toContain('path = /home/u/.local/state/convoy');
    expect(conf).toContain('read only = false');
    expect(conf).toContain('use chroot = false');
  });

  it('fabricExposeArgs registers the per-dial rsync exec handler', () => {
    expect(fabricExposeArgs('/etc/st/rsyncd.conf')).toEqual([
      'expose',
      'st-sync',
      '--exec',
      '--',
      'rsync',
      '--server',
      '--daemon',
      '--config=/etc/st/rsyncd.conf',
      '.',
    ]);
  });
});
