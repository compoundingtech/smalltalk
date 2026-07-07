// tests/unit/overview.test.ts — `st overview` synthesized dashboard.

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

import { cmdOverview } from '../../src/commands/overview.ts';

let scratch: string;
let stRoot: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-overview-test-'));
  stRoot = join(scratch, 'coord');
  mkdirSync(stRoot, { recursive: true });
});
afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

function setupIdentity(id: string): void {
  mkdirSync(join(stRoot, id, 'inbox'), { recursive: true });
  mkdirSync(join(stRoot, id, 'archive'), { recursive: true });
}

function envFor(id: string): NodeJS.ProcessEnv {
  return { ST_AGENT: id } as NodeJS.ProcessEnv;
}

function plantInbox(
  recipient: string,
  filename: string,
  from: string,
  subject: string | undefined,
  mtimeSec?: number
): void {
  let head = `---\nfrom: ${from}\n`;
  if (subject !== undefined) head += `subject: ${subject}\n`;
  head += '---\n';
  const path = join(stRoot, recipient, 'inbox', filename);
  writeFileSync(path, head + 'body\n');
  if (mtimeSec !== undefined) utimesSync(path, mtimeSec, mtimeSec);
}

// ─── Basic shape ────────────────────────────────────────────────────────

describe('cmdOverview — shape', () => {
  it('empty $ST_ROOT just contains the self identity → empty inbox + no members other than self + no recent', () => {
    setupIdentity('operator');
    const r = cmdOverview({ env: envFor('operator'), stRoot });
    expect(r.identity).toBe('operator');
    expect(r.inbox.unread).toBe(0);
    expect(r.inbox.oldest).toBeNull();
    expect(r.members.map((m) => m.identity)).toEqual(['operator']);
    expect(r.recent).toEqual([]);
  });

  it('JSON shape covers every documented field', () => {
    setupIdentity('operator');
    setupIdentity('alice');
    const r = cmdOverview({ env: envFor('operator'), stRoot });
    expect(typeof r.identity).toBe('string');
    expect(typeof r.inbox.unread).toBe('number');
    expect(Array.isArray(r.members)).toBe(true);
    expect(Array.isArray(r.recent)).toBe(true);
    for (const m of r.members) {
      expect(typeof m.identity).toBe('string');
      expect(typeof m.status).toBe('string');
      expect('lastActivity' in m).toBe(true);
      expect(typeof m.inbox).toBe('number');
    }
  });
});

// ─── Inbox summary ──────────────────────────────────────────────────────

describe('cmdOverview — inbox summary', () => {
  it('counts unread matches `st ls --count` semantics (valid filenames only)', () => {
    setupIdentity('operator');
    plantInbox('operator', '1714826789010-aaaaaa.md', 'alice', 'q1');
    plantInbox('operator', '1714826789020-bbbbbb.md', 'bob', 'q2');
    writeFileSync(join(stRoot, 'operator', 'inbox', 'noise.md'), 'x');
    const r = cmdOverview({ env: envFor('operator'), stRoot });
    expect(r.inbox.unread).toBe(2);
  });

  it('oldest item carries filename + from + subject + ageMs (chronological by filename)', () => {
    setupIdentity('operator');
    plantInbox(
      'operator',
      '1714826789010-aaaaaa.md',
      'alice',
      'first question'
    );
    plantInbox(
      'operator',
      '1714826789020-bbbbbb.md',
      'bob',
      'second question'
    );
    const fixedNow = 1714826800000;
    const r = cmdOverview({
      env: envFor('operator'),
      stRoot,
      now: () => fixedNow,
    });
    expect(r.inbox.oldest).toEqual({
      filename: '1714826789010-aaaaaa.md',
      from: 'alice',
      subject: 'first question',
      ageMs: fixedNow - 1714826789010,
    });
  });

  it('inbox.oldest is null when no valid files', () => {
    setupIdentity('operator');
    writeFileSync(join(stRoot, 'operator', 'inbox', 'noise.md'), 'x');
    const r = cmdOverview({ env: envFor('operator'), stRoot });
    expect(r.inbox.oldest).toBeNull();
  });

  it('inbox.oldest from-frontmatter missing → "unknown"', () => {
    setupIdentity('operator');
    writeFileSync(
      join(stRoot, 'operator', 'inbox', '1714826789010-aaaaaa.md'),
      'no fence here\n'
    );
    const r = cmdOverview({ env: envFor('operator'), stRoot });
    expect(r.inbox.oldest?.from).toBe('unknown');
  });
});

// ─── Members section ────────────────────────────────────────────────────

describe('cmdOverview — members section', () => {
  it('includes every identity under $ST_ROOT (self + peers)', () => {
    setupIdentity('operator');
    setupIdentity('alice');
    setupIdentity('bob');
    const r = cmdOverview({ env: envFor('operator'), stRoot });
    expect(r.members.map((m) => m.identity).sort()).toEqual([
      'alice',
      'bob',
      'operator',
    ]);
  });

  it('member with no status file reports offline', () => {
    setupIdentity('operator');
    setupIdentity('alice');
    const r = cmdOverview({ env: envFor('operator'), stRoot });
    expect(r.members.find((m) => m.identity === 'alice')?.status).toBe(
      'offline'
    );
  });

});

// ─── Recent activity ────────────────────────────────────────────────────

describe('cmdOverview — recent activity', () => {
  it('returns the top N entries sorted by mtime desc', () => {
    setupIdentity('operator');
    setupIdentity('alice');
    // Plant five inbox files at strictly-increasing mtimes.
    for (let i = 1; i <= 5; i++) {
      const fn = `${1000 + i}000000000-zzzzz${i}.md`;
      const path = join(stRoot, 'operator', 'inbox', fn);
      writeFileSync(path, `---\nfrom: alice\n---\nb${i}\n`);
      utimesSync(path, i * 1000, i * 1000);
    }
    const r = cmdOverview({
      env: envFor('operator'),
      recent: 3,
      stRoot,
      now: () => 10_000_000,
    });
    expect(r.recent).toHaveLength(3);
    // Newest first — filename suffix 5, 4, 3.
    expect(r.recent[0]!.filename).toContain('zzzzz5');
    expect(r.recent[1]!.filename).toContain('zzzzz4');
    expect(r.recent[2]!.filename).toContain('zzzzz3');
  });

  it('tags entries with the right kind: message / archive / status', () => {
    setupIdentity('operator');
    setupIdentity('alice');
    plantInbox('operator', '1714826789010-aaaaaa.md', 'alice', 'm');
    writeFileSync(
      join(stRoot, 'operator', 'archive', '1714826789020-bbbbbb.md'),
      '---\nfrom: alice\n---\na\n'
    );
    writeFileSync(join(stRoot, 'operator', 'status'), 'busy\n');
    const r = cmdOverview({ env: envFor('operator'), stRoot });
    const kinds = new Set(r.recent.map((a) => a.kind));
    expect(kinds.has('message')).toBe(true);
    expect(kinds.has('archive')).toBe(true);
    expect(kinds.has('status')).toBe(true);
  });

  it('--recent 0 returns an empty recent list', () => {
    setupIdentity('operator');
    plantInbox('operator', '1714826789010-aaaaaa.md', 'alice', 'x');
    const r = cmdOverview({
      env: envFor('operator'),
      recent: 0,
      stRoot,
    });
    expect(r.recent).toEqual([]);
  });

  it('messages carry (sender=identity, recipient=target) + subject', () => {
    setupIdentity('operator');
    setupIdentity('alice');
    plantInbox('operator', '1714826789010-aaaaaa.md', 'alice', 'hi');
    const r = cmdOverview({ env: envFor('operator'), stRoot });
    const msg = r.recent.find((a) => a.kind === 'message');
    expect(msg).toBeDefined();
    expect(msg?.identity).toBe('alice');
    expect(msg?.target).toBe('operator');
    expect(msg?.subject).toBe('hi');
  });
});
