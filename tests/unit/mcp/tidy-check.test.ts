// tests/unit/mcp/tidy-check.test.ts — drift detector (brief-030 task 1).
//
// Pure-function tests against /tmp fixtures. Inbox-staleness is the
// sole drift condition post-brief-009. The tick wiring + dedup
// machinery lives in src/mcp/index.ts and is covered by the
// integration test in tests/integration/mcp-tidy-check.test.ts.

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

import { STALE_INBOX_MS } from '../../../src/common.ts';
import { evaluateDrift } from '../../../src/mcp/tidy-check.ts';

let scratch: string;
let stRoot: string;
let identityRoot: string;

const ID = 'alice';

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'st-tidy-unit-'));
  stRoot = join(scratch, 'smalltalk');
  mkdirSync(join(stRoot, ID, 'inbox'), { recursive: true });
  mkdirSync(join(stRoot, ID, 'archive'), { recursive: true });
  identityRoot = join(stRoot, ID);
});

afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

function plantInbox(filename: string, ageMs: number): string {
  const path = join(identityRoot, 'inbox', filename);
  writeFileSync(path, '---\nfrom: bob\n---\nbody\n');
  if (ageMs > 0) {
    const t = new Date(Date.now() - ageMs);
    utimesSync(path, t, t);
  }
  return path;
}

// ─── Clean / fresh cases ───────────────────────────────────────────────

describe('evaluateDrift — clean fixtures', () => {
  it('empty identity → no drift', () => {
    const r = evaluateDrift(ID, stRoot);
    expect(r.inbox).toBe(false);
    expect(r.body).toBe('');
  });

  it('inbox file fresher than STALE_INBOX_MS → no inbox drift', () => {
    plantInbox('1714826789010-aaaaaa.md', STALE_INBOX_MS - 60_000);
    const r = evaluateDrift(ID, stRoot);
    expect(r.inbox).toBe(false);
  });
});

// ─── Inbox drift ───────────────────────────────────────────────────────

describe('evaluateDrift — inbox condition', () => {
  it('a single stale inbox file fires inbox drift', () => {
    plantInbox('1714826789010-aaaaaa.md', STALE_INBOX_MS + 60_000);
    const r = evaluateDrift(ID, stRoot);
    expect(r.inbox).toBe(true);
    expect(r.detail.inboxStaleCount).toBe(1);
    expect(r.detail.oldestInboxAgeMs).toBeGreaterThan(STALE_INBOX_MS);
    expect(r.body).toContain('inbox: 1 unaddressed message');
  });

  it('multiple stale files report the count + the oldest age', () => {
    plantInbox('1714826789010-aaaaaa.md', STALE_INBOX_MS + 60_000);
    plantInbox('1714826789020-bbbbbb.md', STALE_INBOX_MS + 30 * 60_000);
    plantInbox('1714826789030-cccccc.md', STALE_INBOX_MS + 5 * 60_000);
    const r = evaluateDrift(ID, stRoot);
    expect(r.inbox).toBe(true);
    expect(r.detail.inboxStaleCount).toBe(3);
    // oldest is the +30min one
    expect(r.detail.oldestInboxAgeMs).toBeGreaterThan(STALE_INBOX_MS + 25 * 60_000);
    expect(r.body).toContain('3 unaddressed messages');
  });

  it('non-grammar files in inbox are skipped', () => {
    plantInbox('1714826789010-aaaaaa.md', STALE_INBOX_MS + 60_000);
    // bare README without the <ts>-<rand6>.md grammar
    const noisePath = join(identityRoot, 'inbox', 'README');
    writeFileSync(noisePath, 'docs');
    const oldT = new Date(Date.now() - STALE_INBOX_MS * 10);
    utimesSync(noisePath, oldT, oldT);
    const r = evaluateDrift(ID, stRoot);
    expect(r.detail.inboxStaleCount).toBe(1);
  });
});

// ─── Body shape ────────────────────────────────────────────────────────

describe('evaluateDrift — body shape', () => {
  it('body is empty when no condition fires', () => {
    const r = evaluateDrift(ID, stRoot);
    expect(r.body).toBe('');
  });

  it('body starts with the tidy-check header when drift fires', () => {
    plantInbox('1714826789010-aaaaaa.md', STALE_INBOX_MS + 60_000);
    const r = evaluateDrift(ID, stRoot);
    expect(r.body.startsWith('Tidy check (drift detected):')).toBe(true);
  });

  it('opts.now overrides Date.now for determinism', () => {
    const path = plantInbox('1714826789010-aaaaaa.md', 0);
    // Set the file's mtime to exactly "STALE_INBOX_MS+1 in the past"
    const baseline = 2_000_000_000_000;
    const pastT = new Date(baseline - STALE_INBOX_MS - 1);
    utimesSync(path, pastT, pastT);
    const r = evaluateDrift(ID, stRoot, { now: () => baseline });
    expect(r.inbox).toBe(true);
  });
});

// ─── Resilience ────────────────────────────────────────────────────────

describe('evaluateDrift — resilience', () => {
  it('missing identity folder → no drift, no throw', () => {
    rmSync(scratch, { recursive: true, force: true });
    // Re-mkdir scratch so afterEach's rmSync doesn't error.
    mkdirSync(scratch);
    const r = evaluateDrift(ID, stRoot);
    expect(r.inbox).toBe(false);
  });
});
