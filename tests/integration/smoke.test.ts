// tests/integration/smoke.test.ts — confirms the integration harness works.
//
// Each integration test should be able to: spawn bin/st with a fresh
// ST_ROOT, capture its output, and inspect the resulting filesystem.
// This file pins those guarantees with the smallest possible scenarios.

import { existsSync } from 'node:fs';
import { afterEach, describe, expect, it } from 'vitest';

import {
  COORD_BIN,
  cleanupRoot,
  listInbox,
  mkIdentity,
  mkRoot,
  runCoord,
  runCoordPty,
  rsyncAvailable,
} from './helpers.ts';

describe('integration smoke', () => {
  it('the bin/st shim exists on disk', () => {
    expect(existsSync(COORD_BIN)).toBe(true);
  });

  it('invokes st with --help and exits 0', () => {
    const r = runCoord(['--help']);
    expect(r.exitCode).toBe(0);
    expect(r.stdout).toContain('usage: st');
  });

  it('st with no args exits 2 and prints usage to stderr', () => {
    const r = runCoord([]);
    expect(r.exitCode).toBe(2);
    expect(r.stderr).toContain('usage: st');
  });

  it('st <unknown> exits 2 with the unknown-subcommand message', () => {
    const r = runCoord(['bogus-cmd']);
    expect(r.exitCode).toBe(2);
    expect(r.stderr).toContain('unknown subcommand: bogus-cmd');
  });

  it('mkRoot + mkIdentity + send writes a real file via the binary', () => {
    const root = mkRoot();
    try {
      mkIdentity(root, 'alice');
      const r = runCoord(['message', 'send', 'bob', '--from', 'alice', '--subject', 'hi'], {
        stRoot: root,
        stdin: 'hello bob',
      });
      expect(r.exitCode).toBe(0);
      const filename = r.stdout.trim();
      expect(filename).toMatch(/^[0-9]{13}-[0-9a-z]{6}\.md$/);
      expect(listInbox(root, 'bob')).toEqual([filename]);
    } finally {
      cleanupRoot(root);
    }
  });

  it('rsync is available on PATH (gates the rsync-dependent suites)', () => {
    expect(rsyncAvailable()).toBe(true);
  });

  // PTY smoke (closes in afterEach to avoid leaking the spawned process).
  describe('PTY harness', () => {
    let sessions: Awaited<ReturnType<typeof runCoordPty>>[] = [];
    afterEach(async () => {
      for (const s of sessions) await s.close();
      sessions = [];
    });

    it('Session.spawn returns a usable session for `st help`', async () => {
      const s = runCoordPty(['help']);
      sessions.push(s);
      const ss = await s.waitForText('usage: st');
      expect(ss.text).toContain('usage: st');
    });
  });
});
