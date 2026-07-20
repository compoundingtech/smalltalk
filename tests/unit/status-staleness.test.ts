// tests/unit/status-staleness.test.ts — mtime-based staleness fallback.
//
// brief-022: a status file whose mtime is older than STATUS_STALE_MS
// reads as `unknown` regardless of recorded value. The recorded value
// is honored only when the file is fresh enough that we trust the
// owning agent is still alive.

import {
  mkdirSync,
  mkdtempSync,
  rmSync,
  utimesSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { STATUS_STALE_MS } from '../../src/common.ts';
import { readIdentityStatus } from '../../src/commands/status.ts';

let scratch: string;
let stRoot: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'st-status-staleness-'));
  stRoot = join(scratch, 'smalltalk');
  mkdirSync(join(stRoot, 'alice'), { recursive: true });
});

afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

function writeStatus(id: string, value: string): string {
  const path = join(stRoot, id, 'status');
  writeFileSync(path, `${value}\n`);
  return path;
}

function backdate(path: string, ageMs: number): void {
  const t = Date.now() - ageMs;
  utimesSync(path, new Date(t), new Date(t));
}

describe('readIdentityStatus — mtime staleness', () => {
  it('fresh file → reads the recorded value', () => {
    writeStatus('alice', 'available');
    expect(readIdentityStatus('alice', stRoot)).toBe('available');
  });

  it('file mtime within the staleness window → recorded value still trusted', () => {
    const path = writeStatus('alice', 'busy');
    backdate(path, STATUS_STALE_MS - 60_000); // 1 min under the threshold
    expect(readIdentityStatus('alice', stRoot)).toBe('busy');
  });

  it('file mtime older than STATUS_STALE_MS → returns `unknown`', () => {
    const path = writeStatus('alice', 'available');
    backdate(path, STATUS_STALE_MS + 60_000); // 1 min past the threshold
    expect(readIdentityStatus('alice', stRoot)).toBe('unknown');
  });

  it('stale-yet-recorded `offline` also surfaces as `unknown`', () => {
    // The point of staleness is "we don't trust what's recorded" — the
    // recorded value doesn't matter, only the mtime.
    const path = writeStatus('alice', 'offline');
    backdate(path, STATUS_STALE_MS + 60_000);
    expect(readIdentityStatus('alice', stRoot)).toBe('unknown');
  });

  it('missing file → `offline` (unchanged from pre-brief-022 behavior)', () => {
    // No status file at all is distinct from a stale one: an agent that
    // never wrote status isn't necessarily dead, it just hasn't booted
    // through the ritual yet. LAYOUT-004 says this case is `offline`.
    expect(readIdentityStatus('alice', stRoot)).toBe('offline');
  });

  it('corrupt contents on a fresh file → `offline` (brief-006 rule)', () => {
    writeStatus('alice', 'garbage-value');
    expect(readIdentityStatus('alice', stRoot)).toBe('offline');
  });

  it('corrupt contents on a stale file → `unknown` (staleness wins)', () => {
    const path = writeStatus('alice', 'garbage-value');
    backdate(path, STATUS_STALE_MS + 60_000);
    // mtime is checked before contents — stale is stale regardless.
    expect(readIdentityStatus('alice', stRoot)).toBe('unknown');
  });
});
