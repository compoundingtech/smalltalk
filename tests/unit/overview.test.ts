// tests/unit/overview.test.ts — `coord overview` synthesized dashboard.

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
let coordRoot: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-overview-test-'));
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

function envFor(id: string): NodeJS.ProcessEnv {
  return { COORD_IDENTITY: id } as NodeJS.ProcessEnv;
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
  const path = join(coordRoot, recipient, 'inbox', filename);
  writeFileSync(path, head + 'body\n');
  if (mtimeSec !== undefined) utimesSync(path, mtimeSec, mtimeSec);
}

// ─── Basic shape ────────────────────────────────────────────────────────

describe('cmdOverview — shape', () => {
  it('empty $COORD_ROOT just contains the self identity → empty inbox + no members other than self + no recent', () => {
    setupIdentity('myobie');
    const r = cmdOverview({ env: envFor('myobie'), coordRoot });
    expect(r.identity).toBe('myobie');
    expect(r.inbox.unread).toBe(0);
    expect(r.inbox.oldest).toBeNull();
    expect(r.members.map((m) => m.identity)).toEqual(['myobie']);
    expect(r.recent).toEqual([]);
  });

  it('JSON shape covers every documented field', () => {
    setupIdentity('myobie');
    setupIdentity('alice');
    const r = cmdOverview({ env: envFor('myobie'), coordRoot });
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
  it('counts unread matches `coord ls --count` semantics (valid filenames only)', () => {
    setupIdentity('myobie');
    plantInbox('myobie', '1714826789010-aaaaaa.md', 'alice', 'q1');
    plantInbox('myobie', '1714826789020-bbbbbb.md', 'bob', 'q2');
    writeFileSync(join(coordRoot, 'myobie', 'inbox', 'noise.md'), 'x');
    const r = cmdOverview({ env: envFor('myobie'), coordRoot });
    expect(r.inbox.unread).toBe(2);
  });

  it('oldest item carries filename + from + subject + ageMs (chronological by filename)', () => {
    setupIdentity('myobie');
    plantInbox(
      'myobie',
      '1714826789010-aaaaaa.md',
      'alice',
      'first question'
    );
    plantInbox(
      'myobie',
      '1714826789020-bbbbbb.md',
      'bob',
      'second question'
    );
    const fixedNow = 1714826800000;
    const r = cmdOverview({
      env: envFor('myobie'),
      coordRoot,
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
    setupIdentity('myobie');
    writeFileSync(join(coordRoot, 'myobie', 'inbox', 'noise.md'), 'x');
    const r = cmdOverview({ env: envFor('myobie'), coordRoot });
    expect(r.inbox.oldest).toBeNull();
  });

  it('inbox.oldest from-frontmatter missing → "unknown"', () => {
    setupIdentity('myobie');
    writeFileSync(
      join(coordRoot, 'myobie', 'inbox', '1714826789010-aaaaaa.md'),
      'no fence here\n'
    );
    const r = cmdOverview({ env: envFor('myobie'), coordRoot });
    expect(r.inbox.oldest?.from).toBe('unknown');
  });
});

// ─── Members section ────────────────────────────────────────────────────

describe('cmdOverview — members section', () => {
  it('includes every identity under $COORD_ROOT (self + peers)', () => {
    setupIdentity('myobie');
    setupIdentity('alice');
    setupIdentity('bob');
    const r = cmdOverview({ env: envFor('myobie'), coordRoot });
    expect(r.members.map((m) => m.identity).sort()).toEqual([
      'alice',
      'bob',
      'myobie',
    ]);
  });

  it('member with no status file reports offline', () => {
    setupIdentity('myobie');
    setupIdentity('alice');
    const r = cmdOverview({ env: envFor('myobie'), coordRoot });
    expect(r.members.find((m) => m.identity === 'alice')?.status).toBe(
      'offline'
    );
  });

});

// ─── Recent activity ────────────────────────────────────────────────────

describe('cmdOverview — recent activity', () => {
  it('returns the top N entries sorted by mtime desc', () => {
    setupIdentity('myobie');
    setupIdentity('alice');
    // Plant five inbox files at strictly-increasing mtimes.
    for (let i = 1; i <= 5; i++) {
      const fn = `${1000 + i}000000000-zzzzz${i}.md`;
      const path = join(coordRoot, 'myobie', 'inbox', fn);
      writeFileSync(path, `---\nfrom: alice\n---\nb${i}\n`);
      utimesSync(path, i * 1000, i * 1000);
    }
    const r = cmdOverview({
      env: envFor('myobie'),
      recent: 3,
      coordRoot,
      now: () => 10_000_000,
    });
    expect(r.recent).toHaveLength(3);
    // Newest first — filename suffix 5, 4, 3.
    expect(r.recent[0]!.filename).toContain('zzzzz5');
    expect(r.recent[1]!.filename).toContain('zzzzz4');
    expect(r.recent[2]!.filename).toContain('zzzzz3');
  });

  it('tags entries with the right kind: message / archive / status', () => {
    setupIdentity('myobie');
    setupIdentity('alice');
    plantInbox('myobie', '1714826789010-aaaaaa.md', 'alice', 'm');
    writeFileSync(
      join(coordRoot, 'myobie', 'archive', '1714826789020-bbbbbb.md'),
      '---\nfrom: alice\n---\na\n'
    );
    writeFileSync(join(coordRoot, 'myobie', 'status'), 'busy\n');
    const r = cmdOverview({ env: envFor('myobie'), coordRoot });
    const kinds = new Set(r.recent.map((a) => a.kind));
    expect(kinds.has('message')).toBe(true);
    expect(kinds.has('archive')).toBe(true);
    expect(kinds.has('status')).toBe(true);
  });

  it('--recent 0 returns an empty recent list', () => {
    setupIdentity('myobie');
    plantInbox('myobie', '1714826789010-aaaaaa.md', 'alice', 'x');
    const r = cmdOverview({
      env: envFor('myobie'),
      recent: 0,
      coordRoot,
    });
    expect(r.recent).toEqual([]);
  });

  it('messages carry (sender=identity, recipient=target) + subject', () => {
    setupIdentity('myobie');
    setupIdentity('alice');
    plantInbox('myobie', '1714826789010-aaaaaa.md', 'alice', 'hi');
    const r = cmdOverview({ env: envFor('myobie'), coordRoot });
    const msg = r.recent.find((a) => a.kind === 'message');
    expect(msg).toBeDefined();
    expect(msg?.identity).toBe('alice');
    expect(msg?.target).toBe('myobie');
    expect(msg?.subject).toBe('hi');
  });
});
