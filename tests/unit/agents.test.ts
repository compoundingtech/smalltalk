// tests/unit/members.test.ts — `coord members` enumeration verb.

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
  cmdMembers,
  listIdentities,
  type MemberSummary,
  type MemberSummaryEnriched,
} from '../../src/commands/agents.ts';

let scratch: string;
let coordRoot: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-members-test-'));
  coordRoot = join(scratch, 'coord');
  mkdirSync(coordRoot, { recursive: true });
});
afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

function setupIdentity(id: string): void {
  mkdirSync(join(coordRoot, id, 'inbox'), { recursive: true });
  mkdirSync(join(coordRoot, id, 'archive'), { recursive: true });
}

function setStatus(id: string, state: string): void {
  writeFileSync(join(coordRoot, id, 'status'), `${state}\n`);
}

// ─── Enumeration ────────────────────────────────────────────────────────

describe('cmdMembers / listIdentities', () => {
  it('empty $COORD_ROOT → [] for items, no throw', () => {
    expect(cmdMembers({ coordRoot }).items).toEqual([]);
    expect(listIdentities(coordRoot)).toEqual([]);
  });

  it('detects identities by inbox-or-archive presence', () => {
    setupIdentity('alice');
    setupIdentity('bob');
    expect(listIdentities(coordRoot)).toEqual(['alice', 'bob']);
  });

  it('accepts an identity with ONLY inbox/ (lazy peer from a single send)', () => {
    mkdirSync(join(coordRoot, 'partial', 'inbox'), { recursive: true });
    expect(listIdentities(coordRoot)).toContain('partial');
  });

  it('skips dotfile entries at the root', () => {
    setupIdentity('alice');
    mkdirSync(join(coordRoot, '.hidden'), { recursive: true });
    expect(listIdentities(coordRoot)).toEqual(['alice']);
  });

  it('skips non-directory entries at the root', () => {
    setupIdentity('alice');
    writeFileSync(join(coordRoot, 'looseFile.md'), 'noise');
    expect(listIdentities(coordRoot)).toEqual(['alice']);
  });

  it('skips reserved names at the root', () => {
    setupIdentity('alice');
    // A bare `members/` at the root would be a misnamed identity.
    mkdirSync(join(coordRoot, 'members', 'inbox'), { recursive: true });
    expect(listIdentities(coordRoot)).toEqual(['alice']);
  });

  it('skips invalid-identity-grammar names (uppercase, underscore, etc)', () => {
    setupIdentity('alice');
    mkdirSync(join(coordRoot, 'NOT_VALID', 'inbox'), { recursive: true });
    expect(listIdentities(coordRoot)).toEqual(['alice']);
  });

  it('sorts alphabetically', () => {
    setupIdentity('charlie');
    setupIdentity('alice');
    setupIdentity('bob');
    expect(cmdMembers({ coordRoot }).items.map((m) => m.identity)).toEqual([
      'alice',
      'bob',
      'charlie',
    ]);
  });

  it('an empty-but-existing identity folder (no inbox/archive) is NOT enumerated', () => {
    mkdirSync(join(coordRoot, 'orphan'), { recursive: true });
    expect(listIdentities(coordRoot)).toEqual([]);
  });
});

// ─── Status + name fields ───────────────────────────────────────────────

describe('cmdMembers — status + name fields', () => {
  it('status defaults to offline when no status file', () => {
    setupIdentity('alice');
    const r = cmdMembers({ coordRoot });
    expect(r.items[0]).toMatchObject({
      identity: 'alice',
      status: 'offline',
      name: null,
    });
  });

  it('reads each identity status from its status file', () => {
    setupIdentity('alice');
    setupIdentity('bob');
    setStatus('alice', 'available');
    setStatus('bob', 'busy');
    const byId = mapBy(cmdMembers({ coordRoot }).items);
    expect(byId.alice!.status).toBe('available');
    expect(byId.bob!.status).toBe('busy');
  });

  it('malformed status file → offline (does not crash)', () => {
    setupIdentity('alice');
    writeFileSync(join(coordRoot, 'alice', 'status'), '???garbage???\n');
    expect(cmdMembers({ coordRoot }).items[0]!.status).toBe('offline');
  });

  it('reads display name from <id>/name', () => {
    setupIdentity('alice');
    writeFileSync(join(coordRoot, 'alice', 'name'), 'Alice Awesome\n');
    expect(cmdMembers({ coordRoot }).items[0]!.name).toBe('Alice Awesome');
  });

  it('empty name file → null', () => {
    setupIdentity('alice');
    writeFileSync(join(coordRoot, 'alice', 'name'), '\n');
    expect(cmdMembers({ coordRoot }).items[0]!.name).toBeNull();
  });
});

// ─── --status filter ───────────────────────────────────────────────────

describe('cmdMembers — --status filter', () => {
  beforeEach(() => {
    setupIdentity('alice');
    setupIdentity('bob');
    setupIdentity('carol');
    setStatus('alice', 'available');
    setStatus('bob', 'busy');
    // carol has no status file → offline
  });

  it('filters to a single status', () => {
    expect(
      cmdMembers({ status: 'available', coordRoot }).items.map(
        (m) => m.identity
      )
    ).toEqual(['alice']);
    expect(
      cmdMembers({ status: 'offline', coordRoot }).items.map((m) => m.identity)
    ).toEqual(['carol']);
  });

  it('filters to `away` (brief-029)', () => {
    // Override carol's status (was unset → offline above) so we can
    // exercise the new state through the filter without affecting
    // the sibling test's expectations.
    setStatus('carol', 'away');
    expect(
      cmdMembers({ status: 'away', coordRoot }).items.map((m) => m.identity)
    ).toEqual(['carol']);
  });

  it('empty filter returns everyone', () => {
    expect(cmdMembers({ coordRoot }).items).toHaveLength(3);
  });
});

// ─── --enrich ──────────────────────────────────────────────────────────

describe('cmdMembers --enrich', () => {
  it('lastActivity is null when nothing has been written under <id>/', () => {
    setupIdentity('quiet');
    const r = cmdMembers({ enrich: true, coordRoot });
    const m = r.items[0] as MemberSummaryEnriched;
    expect(m.lastActivity).toBeNull();
  });

  it('lastActivity is the newest mtime across inbox + archive + status', () => {
    setupIdentity('alice');
    // Plant three files at staggered mtimes; the newest should be
    // the value lastActivity returns.
    const inboxFile = join(coordRoot, 'alice', 'inbox', '1.md');
    writeFileSync(inboxFile, 'a');
    utimesSync(inboxFile, 1000, 1000);
    const archiveFile = join(coordRoot, 'alice', 'archive', '2.md');
    writeFileSync(archiveFile, 'b');
    utimesSync(archiveFile, 2000, 2000);
    const statusFile = join(coordRoot, 'alice', 'status');
    writeFileSync(statusFile, 'available\n');
    utimesSync(statusFile, 5000, 5000);
    const r = cmdMembers({ enrich: true, coordRoot });
    const m = r.items[0] as MemberSummaryEnriched;
    // Allow ~ms imprecision — mtimeMs is millisecond-resolution
    // and our utimesSync values are seconds. 5000 seconds = 5e6 ms.
    expect(m.lastActivity).toBeGreaterThanOrEqual(5_000_000);
    expect(m.lastActivity).toBeLessThan(5_001_000);
  });

  it('inbox count matches the valid-filename count', () => {
    setupIdentity('alice');
    writeFileSync(
      join(coordRoot, 'alice', 'inbox', '1714826789010-aaaaaa.md'),
      '---\nfrom: bob\n---\nx\n'
    );
    writeFileSync(
      join(coordRoot, 'alice', 'inbox', '1714826789020-bbbbbb.md'),
      '---\nfrom: bob\n---\ny\n'
    );
    // Noise file that doesn't match the grammar — should be skipped.
    writeFileSync(join(coordRoot, 'alice', 'inbox', 'noise.md'), 'z');
    const r = cmdMembers({ enrich: true, coordRoot });
    const m = r.items[0] as MemberSummaryEnriched;
    expect(m.inbox).toBe(2);
  });

  it('inbox count is 0 when inbox folder missing entirely', () => {
    mkdirSync(join(coordRoot, 'archived', 'archive'), { recursive: true });
    const r = cmdMembers({ enrich: true, coordRoot });
    expect((r.items[0] as MemberSummaryEnriched).inbox).toBe(0);
  });
});

function mapBy(items: MemberSummary[]): Record<string, MemberSummary> {
  return Object.fromEntries(items.map((m) => [m.identity, m]));
}
