// tests/integration/liveness-sync.test.ts — the GATING test for item 1 of the
// cross-machine liveness build: the per-agent `status` + `host` files sync over
// the fabric bus MTIME-PRESERVING, so a remote reader sees real live/offline.
//
// The load-bearing property (the whole design hinges on it): rsync `-t`
// preserves the SOURCE mtime, so a DEAD agent's frozen status mtime propagates
// as frozen — it is NEVER refreshed to receive-time (which would make every
// agent look alive forever). `-u` (newer-wins) keeps a live agent's fresh
// heartbeat from being reverted by the older copy the home host synced out.

import {
  existsSync,
  mkdirSync,
  readFileSync,
  statSync,
  utimesSync,
  writeFileSync,
} from 'node:fs';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { fabricSyncCycle } from '../../src/commands/sync-fabric.ts';
import { cleanupRoot, mkIdentity, mkRoot, modernRsyncAvailable } from './helpers.ts';

const d = modernRsyncAvailable() ? describe : describe.skip;

const DEAD_MTIME_S = 1_600_000_000; // fixed old epoch (2020-09-13) = "agent died long ago"

function statusFile(root: string, id: string): string {
  return join(root, id, 'status');
}
function hostFile(root: string, id: string): string {
  return join(root, id, 'host');
}
function mtimeS(p: string): number {
  return Math.floor(statSync(p).mtimeMs / 1000);
}

d('liveness sync: status + host mtime-preserving (real rsync)', () => {
  let local: string;
  let remote: string;

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

  it('syncs status content + host, and lands them on the remote', () => {
    writeFileSync(statusFile(local, 'bob'), 'busy');
    writeFileSync(hostFile(local, 'bob'), 'macbook');
    fabricSyncCycle(local, `${remote}/`, [], 'inbox-archive');
    expect(readFileSync(statusFile(remote, 'bob'), 'utf8')).toBe('busy');
    expect(readFileSync(hostFile(remote, 'bob'), 'utf8')).toBe('macbook');
  });

  it('PIN: a dead agent’s frozen status mtime propagates FROZEN — never receive-time', () => {
    writeFileSync(statusFile(local, 'bob'), 'available');
    utimesSync(statusFile(local, 'bob'), DEAD_MTIME_S, DEAD_MTIME_S);

    fabricSyncCycle(local, `${remote}/`, [], 'inbox-archive');

    // The receiver's mtime == the SOURCE's frozen mtime, NOT now/receive-time.
    expect(mtimeS(statusFile(remote, 'bob'))).toBe(DEAD_MTIME_S);
    const nowS = Math.floor(Date.now() / 1000);
    // Provably stale by years — a reader would (correctly) read this as dead.
    expect(nowS - mtimeS(statusFile(remote, 'bob'))).toBeGreaterThan(1_000_000);

    // Repeated syncs keep it frozen (never bumped to receive-time).
    fabricSyncCycle(local, `${remote}/`, [], 'inbox-archive');
    fabricSyncCycle(local, `${remote}/`, [], 'inbox-archive');
    expect(mtimeS(statusFile(remote, 'bob'))).toBe(DEAD_MTIME_S);
  });

  it('a live heartbeat (touched source mtime) propagates fresh AND is not reverted by the pull', () => {
    writeFileSync(statusFile(local, 'bob'), 'available');
    utimesSync(statusFile(local, 'bob'), DEAD_MTIME_S, DEAD_MTIME_S);
    fabricSyncCycle(local, `${remote}/`, [], 'inbox-archive');
    expect(mtimeS(statusFile(remote, 'bob'))).toBe(DEAD_MTIME_S);

    // Heartbeat: the home host touches its own agent's status to "now".
    const freshS = Math.floor(Date.now() / 1000);
    utimesSync(statusFile(local, 'bob'), freshS, freshS);

    // The cycle pulls first (remote→local). Without -u, the older remote copy
    // (DEAD_MTIME_S) would overwrite the fresh local touch. With -u it must not.
    fabricSyncCycle(local, `${remote}/`, [], 'inbox-archive');

    expect(mtimeS(statusFile(local, 'bob'))).toBe(freshS); // heartbeat NOT reverted
    expect(mtimeS(statusFile(remote, 'bob'))).toBe(freshS); // and it propagated
  });

  it('does NOT sync context — it stays machine-local under the default scope', () => {
    writeFileSync(statusFile(local, 'bob'), 'available');
    mkdirSync(join(local, 'bob', 'context'), { recursive: true });
    writeFileSync(join(local, 'bob', 'context', 'now.md'), 'machine-local secret');

    fabricSyncCycle(local, `${remote}/`, [], 'inbox-archive');

    expect(existsSync(statusFile(remote, 'bob'))).toBe(true); // status synced
    expect(existsSync(join(remote, 'bob', 'context', 'now.md'))).toBe(false); // context did NOT
  });
});
