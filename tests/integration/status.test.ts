// tests/integration/status.test.ts — status get/set across two machines.
//
// `<id>/status` is a regular file inside the synced tree (LAYOUT-004).
// rsync copies it like every other file; there's no separate status
// protocol. These tests pin that property end to end.

import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { afterAll, beforeEach, describe, expect, it } from 'vitest';

import {
  cleanupRoot,
  mkIdentity,
  mkRoot,
  rsyncAvailable,
  runSt,
} from './helpers.ts';

const skip = !rsyncAvailable();
const d = skip ? describe.skip : describe;

d('status round-trip across machines', () => {
  let A: string;
  let B: string;
  const allRoots: string[] = [];

  beforeEach(() => {
    A = mkRoot();
    B = mkRoot();
    mkIdentity(A, 'alice');
    mkIdentity(B, 'bob');
    allRoots.push(A, B);
  });
  afterAll(() => {
    for (const r of allRoots) cleanupRoot(r);
  });

  it('A sets status; sync; B reads the new state', () => {
    const set = runSt(['status', '--set', 'busy'], {
      stRoot: A,
      stIdentity: 'alice',
    });
    expect(set.exitCode).toBe(0);
    expect(set.stdout.trim()).toBe('status: busy');
    expect(readFileSync(join(A, 'alice', 'status'), 'utf8')).toBe('busy\n');

    runSt(['sync', 'push', `local:${B}`], {
      stRoot: A,
      stIdentity: 'alice',
    });

    // B can read alice's status via its synced view of alice's folder.
    expect(existsSync(join(B, 'alice', 'status'))).toBe(true);
    const get = runSt(['status', 'alice'], {
      stRoot: B,
      stIdentity: 'bob',
    });
    expect(get.exitCode).toBe(0);
    expect(get.stdout.trim()).toBe('busy');
  });

  it('B status defaults to offline when no status file exists', () => {
    const r = runSt(['status'], {
      stRoot: B,
      stIdentity: 'bob',
    });
    expect(r.exitCode).toBe(0);
    expect(r.stdout.trim()).toBe('offline');
    expect(existsSync(join(B, 'bob', 'status'))).toBe(false);
  });

  it('A sets offline; sync; B still reads offline (default == explicit)', () => {
    runSt(['status', '--set', 'offline'], {
      stRoot: A,
      stIdentity: 'alice',
    });
    expect(readFileSync(join(A, 'alice', 'status'), 'utf8')).toBe('offline\n');

    runSt(['sync', 'push', `local:${B}`], {
      stRoot: A,
      stIdentity: 'alice',
    });

    const get = runSt(['status', 'alice'], {
      stRoot: B,
      stIdentity: 'bob',
    });
    expect(get.exitCode).toBe(0);
    expect(get.stdout.trim()).toBe('offline');
  });

  it('bidirectional: both machines set their own status, both files survive', () => {
    runSt(['status', '--set', 'busy'], {
      stRoot: A,
      stIdentity: 'alice',
    });
    runSt(['status', '--set', 'dnd'], {
      stRoot: B,
      stIdentity: 'bob',
    });

    runSt(['sync', 'push', `local:${B}`], {
      stRoot: A,
      stIdentity: 'alice',
    });
    runSt(['sync', 'pull', `local:${B}`], {
      stRoot: A,
      stIdentity: 'alice',
    });
    runSt(['sync', 'push', `local:${A}`], {
      stRoot: B,
      stIdentity: 'bob',
    });
    runSt(['sync', 'pull', `local:${A}`], {
      stRoot: B,
      stIdentity: 'bob',
    });

    // Each machine has both status files intact (no conflict possible —
    // each `<id>/status` is per-identity).
    expect(readFileSync(join(A, 'alice', 'status'), 'utf8')).toBe('busy\n');
    expect(readFileSync(join(A, 'bob', 'status'), 'utf8')).toBe('dnd\n');
    expect(readFileSync(join(B, 'alice', 'status'), 'utf8')).toBe('busy\n');
    expect(readFileSync(join(B, 'bob', 'status'), 'utf8')).toBe('dnd\n');
  });

  it('status file content "garbage" reads as offline (LAYOUT-defined normalize)', () => {
    require('node:fs').writeFileSync(
      join(B, 'bob', 'status'),
      'garbage\n'
    );
    const r = runSt(['status'], {
      stRoot: B,
      stIdentity: 'bob',
    });
    expect(r.exitCode).toBe(0);
    expect(r.stdout.trim()).toBe('offline');
  });

  it('cross-machine: invalid status content on A still normalizes to offline on B', () => {
    require('node:fs').writeFileSync(
      join(A, 'alice', 'status'),
      'BUSY\n' // uppercase = not a valid state per LAYOUT
    );
    runSt(['sync', 'push', `local:${B}`], {
      stRoot: A,
      stIdentity: 'alice',
    });

    const r = runSt(['status', 'alice'], {
      stRoot: B,
      stIdentity: 'bob',
    });
    expect(r.exitCode).toBe(0);
    expect(r.stdout.trim()).toBe('offline');
  });
});
