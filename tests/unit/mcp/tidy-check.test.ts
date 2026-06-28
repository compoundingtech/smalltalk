// tests/unit/mcp/tidy-check.test.ts — drift detector (brief-030 task 1).
//
// Pure-function tests against /tmp fixtures. Each drift condition is
// exercised in isolation, then combined. The tick wiring + dedup
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

import {
  STALE_INBOX_MS,
  STALE_JOURNAL_MS,
} from '../../../src/common.ts';
import { evaluateDrift } from '../../../src/mcp/tidy-check.ts';

let scratch: string;
let coordRoot: string;
let identityRoot: string;

const ID = 'alice';

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-tidy-unit-'));
  coordRoot = join(scratch, 'coord');
  mkdirSync(join(coordRoot, ID, 'inbox'), { recursive: true });
  mkdirSync(join(coordRoot, ID, 'archive'), { recursive: true });
  identityRoot = join(coordRoot, ID);
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

function plantJournal(filename: string, ageMs: number): string {
  const dir = join(identityRoot, 'journal');
  mkdirSync(dir, { recursive: true });
  const path = join(dir, filename);
  writeFileSync(path, 'entry body\n');
  if (ageMs > 0) {
    const t = new Date(Date.now() - ageMs);
    utimesSync(path, t, t);
  }
  return path;
}

// ─── Empty / clean cases ───────────────────────────────────────────────

describe('evaluateDrift — clean fixtures', () => {
  it('empty identity → no drift', () => {
    const r = evaluateDrift(ID, coordRoot);
    expect(r.inbox).toBe(false);
    expect(r.journal).toBe(false);
    expect(r.body).toBe('');
  });

  it('inbox file fresher than STALE_INBOX_MS → no inbox drift', () => {
    plantInbox('1714826789010-aaaaaa.md', STALE_INBOX_MS - 60_000);
    const r = evaluateDrift(ID, coordRoot);
    expect(r.inbox).toBe(false);
  });

  it('journal entry fresher than STALE_JOURNAL_MS → no journal drift', () => {
    plantJournal('1714826789010-recent.md', STALE_JOURNAL_MS - 60_000);
    const r = evaluateDrift(ID, coordRoot);
    expect(r.journal).toBe(false);
  });
});

// ─── Inbox drift ───────────────────────────────────────────────────────

describe('evaluateDrift — inbox condition', () => {
  it('a single stale inbox file fires inbox drift', () => {
    plantInbox('1714826789010-aaaaaa.md', STALE_INBOX_MS + 60_000);
    const r = evaluateDrift(ID, coordRoot);
    expect(r.inbox).toBe(true);
    expect(r.detail.inboxStaleCount).toBe(1);
    expect(r.detail.oldestInboxAgeMs).toBeGreaterThan(STALE_INBOX_MS);
    expect(r.body).toContain('inbox: 1 unaddressed message');
  });

  it('multiple stale files report the count + the oldest age', () => {
    plantInbox('1714826789010-aaaaaa.md', STALE_INBOX_MS + 60_000);
    plantInbox('1714826789020-bbbbbb.md', STALE_INBOX_MS + 30 * 60_000);
    plantInbox('1714826789030-cccccc.md', STALE_INBOX_MS + 5 * 60_000);
    const r = evaluateDrift(ID, coordRoot);
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
    const r = evaluateDrift(ID, coordRoot);
    expect(r.detail.inboxStaleCount).toBe(1);
  });
});

// ─── Journal drift ─────────────────────────────────────────────────────

describe('evaluateDrift — journal condition', () => {
  it('latest journal entry older than STALE_JOURNAL_MS → fires', () => {
    plantJournal(
      '1714826789010-old.md',
      STALE_JOURNAL_MS + 30 * 60_000
    );
    const r = evaluateDrift(ID, coordRoot);
    expect(r.journal).toBe(true);
    expect(r.body).toContain('No journal entry for');
  });

  it('latest journal entry fresher than threshold → no drift', () => {
    plantJournal(
      '1714826789020-recent.md',
      STALE_JOURNAL_MS - 60_000
    );
    const r = evaluateDrift(ID, coordRoot);
    expect(r.journal).toBe(false);
  });

  it('no journal folder → no journal drift (nothing to be stale)', () => {
    // Bare identity with no work yet shouldn't fire.
    const r = evaluateDrift(ID, coordRoot);
    expect(r.journal).toBe(false);
  });

  it('latest of multiple entries is what we compare against', () => {
    plantJournal(
      '1714826789010-old.md',
      STALE_JOURNAL_MS + 5 * 60 * 60_000
    );
    plantJournal(
      '1714826789020-fresh.md',
      STALE_JOURNAL_MS - 60_000
    );
    const r = evaluateDrift(ID, coordRoot);
    // Newest entry is fresh, so no drift.
    expect(r.journal).toBe(false);
  });
});

// ─── Combinations + body shape ─────────────────────────────────────────

describe('evaluateDrift — combinations', () => {
  it('both conditions fire together → body lists both', () => {
    plantInbox('1714826789010-aaaaaa.md', STALE_INBOX_MS + 60_000);
    plantJournal(
      '1714826789030-old-journal.md',
      STALE_JOURNAL_MS + 60_000
    );
    const r = evaluateDrift(ID, coordRoot);
    expect(r.inbox).toBe(true);
    expect(r.journal).toBe(true);
    expect(r.body).toContain('inbox:');
    expect(r.body).toContain('No journal entry for');
    expect(r.body.startsWith('Tidy check (drift detected):')).toBe(true);
  });

  it('body is empty when no condition fires', () => {
    const r = evaluateDrift(ID, coordRoot);
    expect(r.body).toBe('');
  });

  it('opts.now overrides Date.now for determinism', () => {
    const path = plantInbox('1714826789010-aaaaaa.md', 0);
    // Set the file's mtime to exactly "STALE_INBOX_MS+1 in the past"
    const baseline = 2_000_000_000_000;
    const pastT = new Date(baseline - STALE_INBOX_MS - 1);
    utimesSync(path, pastT, pastT);
    const r = evaluateDrift(ID, coordRoot, { now: () => baseline });
    expect(r.inbox).toBe(true);
  });
});

// ─── Resilience ────────────────────────────────────────────────────────

describe('evaluateDrift — resilience', () => {
  it('missing identity folder → no drift, no throw', () => {
    rmSync(scratch, { recursive: true, force: true });
    // Re-mkdir scratch so afterEach's rmSync doesn't error.
    mkdirSync(scratch);
    const r = evaluateDrift(ID, coordRoot);
    expect(r.inbox).toBe(false);
    expect(r.journal).toBe(false);
  });
});
