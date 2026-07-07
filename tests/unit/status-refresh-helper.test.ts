// tests/unit/status-refresh-helper.test.ts — pure refreshIdentityStatus.
//
// brief-032 extracted the brief-023 MCP-server refresh logic into
// `refreshIdentityStatus(identity, root)` in commands/status.ts so
// `st ding` can call it too. This file tests the helper directly;
// the existing brief-023 integration tests (mcp/status-refresh.test.ts)
// continue to pin the MCP-server tick that calls this helper.

import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  utimesSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { refreshIdentityStatus } from '../../src/commands/status.ts';

let scratch: string;
let stRoot: string;
let statusPath: string;

const ID = 'alice';

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-refresh-helper-'));
  stRoot = join(scratch, 'coord');
  mkdirSync(join(stRoot, ID), { recursive: true });
  statusPath = join(stRoot, ID, 'status');
});

afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

describe('refreshIdentityStatus — outcomes', () => {
  it('missing file → wrote-default, file contains `available`', () => {
    const outcome = refreshIdentityStatus(ID, stRoot);
    expect(outcome).toBe('wrote-default');
    expect(readFileSync(statusPath, 'utf8').trim()).toBe('available');
  });

  it('valid settable state → refreshed, value preserved', () => {
    for (const state of ['offline', 'available', 'busy', 'away', 'dnd']) {
      writeFileSync(statusPath, `${state}\n`);
      // Backdate so a mtime advance is observable on any filesystem.
      const past = new Date(Date.now() - 10_000);
      utimesSync(statusPath, past, past);
      const mtimeBefore = statSync(statusPath).mtimeMs;
      const outcome = refreshIdentityStatus(ID, stRoot);
      expect(outcome).toBe('refreshed');
      expect(readFileSync(statusPath, 'utf8').trim()).toBe(state);
      expect(statSync(statusPath).mtimeMs).toBeGreaterThan(mtimeBefore);
    }
  });

  it('literal `unknown` on disk → left-unknown, file untouched', () => {
    writeFileSync(statusPath, 'unknown\n');
    const before = readFileSync(statusPath, 'utf8');
    const outcome = refreshIdentityStatus(ID, stRoot);
    expect(outcome).toBe('left-unknown');
    expect(readFileSync(statusPath, 'utf8')).toBe(before);
  });

  it('corrupt content → left-corrupt, file untouched', () => {
    writeFileSync(statusPath, 'garbage-value\n');
    const before = readFileSync(statusPath, 'utf8');
    const outcome = refreshIdentityStatus(ID, stRoot);
    expect(outcome).toBe('left-corrupt');
    expect(readFileSync(statusPath, 'utf8')).toBe(before);
  });

  it('empty file → left-corrupt (first line is empty, not a valid state)', () => {
    writeFileSync(statusPath, '');
    const outcome = refreshIdentityStatus(ID, stRoot);
    expect(outcome).toBe('left-corrupt');
  });
});

describe('refreshIdentityStatus — atomic write semantics', () => {
  it('the on-disk value is only ever a complete settable state — never partial', () => {
    // 10 refreshes back-to-back. After each one, a concurrent
    // reader should always see a complete state line. Since we
    // rename-from-tmp, a partial line can't appear.
    writeFileSync(statusPath, 'busy\n');
    for (let i = 0; i < 10; i++) {
      const outcome = refreshIdentityStatus(ID, stRoot);
      expect(outcome).toBe('refreshed');
      const content = readFileSync(statusPath, 'utf8');
      expect(content).toBe('busy\n');
    }
  });

  it('no leftover .status.tmp files after a successful refresh', () => {
    writeFileSync(statusPath, 'available\n');
    refreshIdentityStatus(ID, stRoot);
    const { readdirSync } = require('node:fs') as typeof import('node:fs');
    const dir = readdirSync(join(stRoot, ID));
    expect(dir.some((n: string) => n.startsWith('.status.tmp-'))).toBe(false);
  });
});
