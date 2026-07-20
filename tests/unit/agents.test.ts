// tests/unit/members.test.ts — `st members` enumeration verb.

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
  cmdAgentsCli,
  cmdMembers,
  formatAge,
  listIdentities,
  type MemberSummary,
  type MemberSummaryEnriched,
} from '../../src/commands/agents.ts';

let scratch: string;
let stRoot: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'st-members-test-'));
  stRoot = join(scratch, 'smalltalk');
  mkdirSync(stRoot, { recursive: true });
});
afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

function setupIdentity(id: string): void {
  mkdirSync(join(stRoot, id, 'inbox'), { recursive: true });
  mkdirSync(join(stRoot, id, 'archive'), { recursive: true });
}

function setStatus(id: string, state: string): void {
  writeFileSync(join(stRoot, id, 'status'), `${state}\n`);
}

// ─── Enumeration ────────────────────────────────────────────────────────

describe('cmdMembers / listIdentities', () => {
  it('empty $ST_ROOT → [] for items, no throw', () => {
    expect(cmdMembers({ stRoot }).items).toEqual([]);
    expect(listIdentities(stRoot)).toEqual([]);
  });

  it('detects identities by inbox-or-archive presence', () => {
    setupIdentity('alice');
    setupIdentity('bob');
    expect(listIdentities(stRoot)).toEqual(['alice', 'bob']);
  });

  it('accepts an identity with ONLY inbox/ (lazy peer from a single send)', () => {
    mkdirSync(join(stRoot, 'partial', 'inbox'), { recursive: true });
    expect(listIdentities(stRoot)).toContain('partial');
  });

  it('accepts a status-only folder (message-less cross-machine agent whose empty inbox/archive were pruned by rsync --prune-empty-dirs)', () => {
    mkdirSync(join(stRoot, 'hetz.bob'), { recursive: true });
    setStatus('hetz.bob', 'available');
    expect(listIdentities(stRoot)).toContain('hetz.bob');
  });

  it('skips a folder with no inbox/archive AND no status (not an agent)', () => {
    mkdirSync(join(stRoot, 'notanagent'), { recursive: true });
    expect(listIdentities(stRoot)).not.toContain('notanagent');
  });

  it('skips dotfile entries at the root', () => {
    setupIdentity('alice');
    mkdirSync(join(stRoot, '.hidden'), { recursive: true });
    expect(listIdentities(stRoot)).toEqual(['alice']);
  });

  it('skips non-directory entries at the root', () => {
    setupIdentity('alice');
    writeFileSync(join(stRoot, 'looseFile.md'), 'noise');
    expect(listIdentities(stRoot)).toEqual(['alice']);
  });

  it('skips reserved names at the root', () => {
    setupIdentity('alice');
    // A bare `members/` at the root would be a misnamed identity.
    mkdirSync(join(stRoot, 'members', 'inbox'), { recursive: true });
    expect(listIdentities(stRoot)).toEqual(['alice']);
  });

  it('skips invalid-identity-grammar names (uppercase, underscore, etc)', () => {
    setupIdentity('alice');
    mkdirSync(join(stRoot, 'NOT_VALID', 'inbox'), { recursive: true });
    expect(listIdentities(stRoot)).toEqual(['alice']);
  });

  it('sorts alphabetically', () => {
    setupIdentity('charlie');
    setupIdentity('alice');
    setupIdentity('bob');
    expect(cmdMembers({ stRoot }).items.map((m) => m.identity)).toEqual([
      'alice',
      'bob',
      'charlie',
    ]);
  });

  it('an empty-but-existing identity folder (no inbox/archive) is NOT enumerated', () => {
    mkdirSync(join(stRoot, 'orphan'), { recursive: true });
    expect(listIdentities(stRoot)).toEqual([]);
  });
});

// ─── Status + name fields ───────────────────────────────────────────────

describe('cmdMembers — status + name fields', () => {
  it('status defaults to offline when no status file', () => {
    setupIdentity('alice');
    const r = cmdMembers({ stRoot });
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
    const byId = mapBy(cmdMembers({ stRoot }).items);
    expect(byId.alice!.status).toBe('available');
    expect(byId.bob!.status).toBe('busy');
  });

  it('malformed status file → offline (does not crash)', () => {
    setupIdentity('alice');
    writeFileSync(join(stRoot, 'alice', 'status'), '???garbage???\n');
    expect(cmdMembers({ stRoot }).items[0]!.status).toBe('offline');
  });

  it('reads display name from <id>/name', () => {
    setupIdentity('alice');
    writeFileSync(join(stRoot, 'alice', 'name'), 'Alice Awesome\n');
    expect(cmdMembers({ stRoot }).items[0]!.name).toBe('Alice Awesome');
  });

  it('empty name file → null', () => {
    setupIdentity('alice');
    writeFileSync(join(stRoot, 'alice', 'name'), '\n');
    expect(cmdMembers({ stRoot }).items[0]!.name).toBeNull();
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
      cmdMembers({ status: 'available', stRoot }).items.map(
        (m) => m.identity
      )
    ).toEqual(['alice']);
    expect(
      cmdMembers({ status: 'offline', stRoot }).items.map((m) => m.identity)
    ).toEqual(['carol']);
  });

  it('filters to `away` (brief-029)', () => {
    // Override carol's status (was unset → offline above) so we can
    // exercise the new state through the filter without affecting
    // the sibling test's expectations.
    setStatus('carol', 'away');
    expect(
      cmdMembers({ status: 'away', stRoot }).items.map((m) => m.identity)
    ).toEqual(['carol']);
  });

  it('empty filter returns everyone', () => {
    expect(cmdMembers({ stRoot }).items).toHaveLength(3);
  });
});

// ─── --enrich ──────────────────────────────────────────────────────────

describe('cmdMembers --enrich', () => {
  it('lastActivity is null when nothing has been written under <id>/', () => {
    setupIdentity('quiet');
    const r = cmdMembers({ enrich: true, stRoot });
    const m = r.items[0] as MemberSummaryEnriched;
    expect(m.lastActivity).toBeNull();
  });

  it('lastActivity is the newest mtime across inbox + archive + status', () => {
    setupIdentity('alice');
    // Plant three files at staggered mtimes; the newest should be
    // the value lastActivity returns.
    const inboxFile = join(stRoot, 'alice', 'inbox', '1.md');
    writeFileSync(inboxFile, 'a');
    utimesSync(inboxFile, 1000, 1000);
    const archiveFile = join(stRoot, 'alice', 'archive', '2.md');
    writeFileSync(archiveFile, 'b');
    utimesSync(archiveFile, 2000, 2000);
    const statusFile = join(stRoot, 'alice', 'status');
    writeFileSync(statusFile, 'available\n');
    utimesSync(statusFile, 5000, 5000);
    const r = cmdMembers({ enrich: true, stRoot });
    const m = r.items[0] as MemberSummaryEnriched;
    // Allow ~ms imprecision — mtimeMs is millisecond-resolution
    // and our utimesSync values are seconds. 5000 seconds = 5e6 ms.
    expect(m.lastActivity).toBeGreaterThanOrEqual(5_000_000);
    expect(m.lastActivity).toBeLessThan(5_001_000);
  });

  it('inbox count matches the valid-filename count', () => {
    setupIdentity('alice');
    writeFileSync(
      join(stRoot, 'alice', 'inbox', '1714826789010-aaaaaa.md'),
      '---\nfrom: bob\n---\nx\n'
    );
    writeFileSync(
      join(stRoot, 'alice', 'inbox', '1714826789020-bbbbbb.md'),
      '---\nfrom: bob\n---\ny\n'
    );
    // Noise file that doesn't match the grammar — should be skipped.
    writeFileSync(join(stRoot, 'alice', 'inbox', 'noise.md'), 'z');
    const r = cmdMembers({ enrich: true, stRoot });
    const m = r.items[0] as MemberSummaryEnriched;
    expect(m.inbox).toBe(2);
  });

  it('inbox count is 0 when inbox folder missing entirely', () => {
    mkdirSync(join(stRoot, 'archived', 'archive'), { recursive: true });
    const r = cmdMembers({ enrich: true, stRoot });
    expect((r.items[0] as MemberSummaryEnriched).inbox).toBe(0);
  });
});

function mapBy(items: MemberSummary[]): Record<string, MemberSummary> {
  return Object.fromEntries(items.map((m) => [m.identity, m]));
}

// ─── #102: roster freshness ────────────────────────────────────────────
//
// `st agents` printed the derived state bare, so a status file last
// touched minutes ago rendered identically to one touched a second ago.
// A roster command is exactly where "as of when?" matters — this is
// what let `st agents` and `convoy ls --tree` disagree out loud about
// the same identity at the same instant.
//
// NOTE the deliberate non-change: `status` (and therefore what
// `--status` filters on) keeps its old meaning. Only the rendered cell
// gains a qualifier, and --json gains additive fields.
describe('agents: status freshness (#102)', () => {
  function backdateStatus(id: string, ageMs: number): void {
    const p = join(stRoot, id, 'status');
    const t = new Date(Date.now() - ageMs);
    utimesSync(p, t, t);
  }

  function render(): string {
    let out = '';
    cmdAgentsCli([], {
      stRoot,
      env: {},
      stdout: (s: string) => {
        out += s;
      },
      stderr: () => {},
    } as unknown as Parameters<typeof cmdAgentsCli>[1]);
    return out;
  }

  it('fresh status renders bare (no annotation noise on a live roster)', () => {
    setupIdentity('alice');
    setStatus('alice', 'available');
    expect(render()).toBe('alice\tavailable\t\n');
  });

  it('the reported case: 3m-old `available` is annotated, not reported live', () => {
    setupIdentity('alice');
    setStatus('alice', 'available');
    backdateStatus('alice', 3 * 60_000);
    expect(render()).toContain('available (stale 3m)');
  });

  it('past the trust window: shows `unknown` but keeps what it last claimed', () => {
    setupIdentity('alice');
    setStatus('alice', 'busy');
    backdateStatus('alice', 25 * 60_000);
    const out = render();
    expect(out).toContain('unknown (was busy, 25m)');
  });

  it('stale-was-busy is distinguishable from stale-was-offline', () => {
    setupIdentity('alice');
    setStatus('alice', 'busy');
    backdateStatus('alice', 25 * 60_000);
    setupIdentity('carol');
    setStatus('carol', 'offline');
    backdateStatus('carol', 25 * 60_000);
    const out = render();
    expect(out).toContain('was busy');
    expect(out).toContain('was offline');
  });

  it('no status file at all → plain `offline`, nothing to qualify', () => {
    setupIdentity('alice');
    expect(render()).toBe('alice\toffline\t\n');
  });

  it('--status filtering keeps its old meaning (no semantic drift)', () => {
    // The derived `status` field is untouched by the annotation work, so
    // a stale-but-trusted `available` still matches `--status available`.
    setupIdentity('alice');
    setStatus('alice', 'available');
    backdateStatus('alice', 3 * 60_000);
    const items = cmdMembers({ stRoot, status: 'available' }).items;
    expect(items.map((m) => m.identity)).toEqual(['alice']);
  });

  it('--json exposes live / statusMtimeMs / recorded additively', () => {
    setupIdentity('alice');
    setStatus('alice', 'busy');
    backdateStatus('alice', 5 * 60_000);
    const m = cmdMembers({ stRoot }).items[0] as MemberSummary;
    expect(m.status).toBe('busy'); // unchanged
    expect(m.live).toBe(false);
    expect(m.recorded).toBe('busy');
    expect(m.statusMtimeMs).not.toBeNull();
    expect(Date.now() - (m.statusMtimeMs as number)).toBeGreaterThan(
      4 * 60_000
    );
  });

  it('two back-to-back reads of an unchanged bus compare equal', () => {
    // Regression guard: an `ageMs` field here would make every read
    // differ from the last. Absolute mtimes keep the shape repeatable.
    setupIdentity('alice');
    setStatus('alice', 'available');
    expect(cmdMembers({ stRoot }).items).toEqual(cmdMembers({ stRoot }).items);
  });

  it('formatAge renders compact units', () => {
    expect(formatAge(45_000)).toBe('45s');
    expect(formatAge(3 * 60_000)).toBe('3m');
    expect(formatAge(2 * 60 * 60_000)).toBe('2h');
    expect(formatAge(4 * 24 * 60 * 60_000)).toBe('4d');
  });
});
