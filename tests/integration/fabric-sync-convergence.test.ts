// tests/integration/fabric-sync-convergence.test.ts — real rsync acceptance
// for `st sync fabric`'s cycle: ONE bidirectional cycle converges both roots
// (no peer-side zombie) via the client-driven safe remote-sweep, and a fresh,
// unreconciled delivery on either side survives. Contrast the plain-union path
// (two-machine-convergence.test.ts) which needs multiple rounds + a peer-side
// sweep to drain zombies.
//
// The "remote" is a second $ST_ROOT on the same disk, reached as a `local:`
// rsync target (transportArgs = [] — no fabric/socat needed to exercise the
// pull -> sweep-both-roots -> push logic).

import { existsSync, mkdirSync, readdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { fabricSyncCycle } from '../../src/commands/sync-fabric.ts';
import { cleanupRoot, mkIdentity, mkRoot, modernRsyncAvailable } from './helpers.ts';

// Remote sweep needs daemon-over-rsh + protect/risk `--delete`, which
// openrsync (macOS's /usr/bin/rsync) can't do — so SKIP (not FAIL) there,
// mirroring the tool's own runtime openrsync reject.
const d = modernRsyncAvailable() ? describe : describe.skip;

const X = '1714826789010-aaaaaa.md'; // archived message (the zombie candidate)
const Y = '1714826789020-bbbbbb.md'; // fresh delivery originating on A
const Z = '1714826789030-cccccc.md'; // fresh delivery originating on B

function inboxOf(root: string, id: string): string[] {
  const dir = join(root, id, 'inbox');
  return existsSync(dir) ? readdirSync(dir).sort() : [];
}
function archiveOf(root: string, id: string): string[] {
  const dir = join(root, id, 'archive');
  return existsSync(dir) ? readdirSync(dir).sort() : [];
}
function put(root: string, id: string, folder: 'inbox' | 'archive', name: string, body: string): void {
  mkdirSync(join(root, id, folder), { recursive: true });
  writeFileSync(join(root, id, folder, name), body);
}

d('fabric sync cycle convergence (real rsync)', () => {
  let local: string; // the syncer / client (drives the cycle)
  let remote: string; // the peer (holds NO resident proc)

  beforeEach(() => {
    local = mkRoot();
    remote = mkRoot();
    mkIdentity(local, 'bob');
    mkIdentity(remote, 'bob');
  });
  afterEach(() => {
    cleanupRoot(local);
    cleanupRoot(remote);
  });

  it('one cycle drains a peer-side zombie AND preserves fresh deliveries on both sides', () => {
    // local: bob has X delivered+archived (twin), plus a fresh delivery Y.
    put(local, 'bob', 'inbox', X, 'the message');
    put(local, 'bob', 'archive', X, 'the message');
    put(local, 'bob', 'inbox', Y, 'fresh from A');
    // remote: still carries the inbox zombie X (not yet archived there), plus
    // its own fresh delivery Z.
    put(remote, 'bob', 'inbox', X, 'the message');
    put(remote, 'bob', 'inbox', Z, 'fresh from B');

    const r = fabricSyncCycle(local, `${remote}/`, [], 'inbox-archive');

    // The cycle remote-deleted exactly the twin-verified zombie.
    expect(r.remoteDeleted).toEqual([`bob/inbox/${X}`]);

    // Both roots converged in ONE cycle: zombie X gone from BOTH inboxes,
    // the tombstone present on both, and BOTH fresh deliveries on both sides.
    expect(inboxOf(local, 'bob')).toEqual([Y, Z]);
    expect(inboxOf(remote, 'bob')).toEqual([Y, Z]);
    expect(archiveOf(local, 'bob')).toEqual([X]);
    expect(archiveOf(remote, 'bob')).toEqual([X]);
  });

  it('SAFETY under real rsync: a delivery whose archive twin is byte-DIVERGENT is not deleted', () => {
    // local edited its inbox copy so it no longer matches the archive — a
    // violated invariant, not a tombstone. Neither sweep may delete it.
    put(local, 'bob', 'inbox', X, 'EDITED locally');
    put(local, 'bob', 'archive', X, 'original archived');
    put(remote, 'bob', 'inbox', X, 'EDITED locally');

    const r = fabricSyncCycle(local, `${remote}/`, [], 'inbox-archive');

    expect(r.remoteDeleted).toEqual([]); // nothing risked
    expect(inboxOf(local, 'bob')).toContain(X); // survives locally
    expect(inboxOf(remote, 'bob')).toContain(X); // survives on the peer
  });

  it('idempotent: a second cycle on a converged pair deletes nothing new', () => {
    put(local, 'bob', 'inbox', X, 'the message');
    put(local, 'bob', 'archive', X, 'the message');
    put(remote, 'bob', 'inbox', X, 'the message');

    fabricSyncCycle(local, `${remote}/`, [], 'inbox-archive');
    const second = fabricSyncCycle(local, `${remote}/`, [], 'inbox-archive');

    expect(second.remoteDeleted).toEqual([]);
    expect(second.localRemoved).toBe(0);
    expect(inboxOf(local, 'bob')).toEqual([]);
    expect(inboxOf(remote, 'bob')).toEqual([]);
    expect(archiveOf(remote, 'bob')).toEqual([X]);
  });
});
