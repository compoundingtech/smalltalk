// tests/unit/types.test.ts — branded primitives + parsePeer.

import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  asFilename,
  asIdentity,
  deriveTs,
  deriveTo,
  isFilename,
  isIdentity,
  isState,
  parsePeer,
  PRIORITIES,
  STATES,
  type Filename,
  type Identity,
} from '../../src/types.ts';

// ─── isIdentity / asIdentity ────────────────────────────────────────────

describe('isIdentity', () => {
  it.each(['alice', 'pty-relay-claude', 'a', 'a1', '1a'])(
    'accepts %s',
    (s) => {
      expect(isIdentity(s)).toBe(true);
    }
  );

  it.each([
    '',
    'INVALID',
    '-leading',
    'trailing-',
    'inbox',
    'archive',
    'status',
    'name',
    'offline',
    'available',
    'busy',
    'dnd',
  ])('rejects %s', (s) => {
    expect(isIdentity(s)).toBe(false);
  });

  it('narrows the type after the guard', () => {
    const s: string = 'alice';
    if (isIdentity(s)) {
      // Should typecheck as Identity here.
      const id: Identity = s;
      expect(id).toBe('alice');
    } else {
      throw new Error('unreachable');
    }
  });
});

describe('asIdentity', () => {
  it('returns the branded value for a valid name', () => {
    const id = asIdentity('alice');
    expect(id).toBe('alice');
  });

  it('throws on invalid', () => {
    expect(() => asIdentity('INVALID')).toThrowError(/invalid (agent name|identity)/);
  });

  it('throws on reserved name', () => {
    expect(() => asIdentity('inbox')).toThrowError(/invalid (agent name|identity)/);
  });
});

// ─── isFilename / asFilename ────────────────────────────────────────────

describe('isFilename', () => {
  it('accepts the canonical shape', () => {
    expect(isFilename('1714826789012-x9k4mz.md')).toBe(true);
  });

  it.each([
    '',
    'garbage',
    '171482678901-x9k4mz.md', // 12-digit ts
    '1714826789012-x9k4m.md', // 5-char rand
    '1714826789012-X9K4MZ.md', // uppercase
    '1714826789012-myobie-x9k4mz.md', // legacy 3-segment
    ' 1714826789012-x9k4mz.md', // leading whitespace
    '1714826789012-x9k4mz.md\n', // trailing newline
  ])('rejects %s', (s) => {
    expect(isFilename(s)).toBe(false);
  });

  it('narrows the type after the guard', () => {
    const s: string = '1714826789012-x9k4mz.md';
    if (isFilename(s)) {
      const f: Filename = s;
      expect(f).toBe(s);
    } else {
      throw new Error('unreachable');
    }
  });
});

describe('asFilename', () => {
  it('returns the branded value', () => {
    const f = asFilename('1714826789012-x9k4mz.md');
    expect(f).toBe('1714826789012-x9k4mz.md');
  });

  it('throws on garbage', () => {
    expect(() => asFilename('garbage')).toThrowError(/invalid filename/);
  });
});

// ─── isState / STATES / PRIORITIES ──────────────────────────────────────

describe('isState', () => {
  // brief-022: `unknown` IS a known state (derived from mtime
  // staleness). It's just not user-settable — see isSettableState in
  // commands/status.ts.
  // brief-029: `away` joins the settable set.
  it.each(['offline', 'available', 'busy', 'away', 'dnd', 'unknown'])(
    'accepts %s',
    (s) => {
      expect(isState(s)).toBe(true);
    }
  );

  it.each(['', 'AVAILABLE', 'foo'])('rejects %s', (s) => {
    expect(isState(s)).toBe(false);
  });
});

describe('STATES / PRIORITIES constants', () => {
  it('STATES enumerates the LAYOUT states (incl. derived `unknown`)', () => {
    expect([...STATES]).toEqual([
      'offline',
      'available',
      'busy',
      'away',
      'dnd',
      'unknown',
    ]);
  });

  it('PRIORITIES is exactly the canonical three values', () => {
    expect([...PRIORITIES]).toEqual(['low', 'normal', 'high']);
  });
});

// ─── deriveTo / deriveTs ────────────────────────────────────────────────

describe('deriveTo', () => {
  it('returns the identity-folder name as a branded Identity', () => {
    const f = asFilename('1714826789012-x9k4mz.md');
    expect(deriveTo(f, 'bob')).toBe('bob');
  });

  it('throws when the path piece is not a valid identity', () => {
    const f = asFilename('1714826789012-x9k4mz.md');
    expect(() => deriveTo(f, 'INVALID')).toThrowError(/invalid (agent name|identity)/);
  });
});

describe('deriveTs', () => {
  it('extracts the unix-ms prefix as a number', () => {
    expect(deriveTs(asFilename('1714826789012-x9k4mz.md'))).toBe(
      1714826789012
    );
  });

  it('rejects an invalid filename', () => {
    expect(() => deriveTs('garbage' as Filename)).toThrowError(
      /invalid filename/
    );
  });
});

// ─── parsePeer ──────────────────────────────────────────────────────────

describe('parsePeer', () => {
  let scratch: string;

  beforeEach(() => {
    scratch = mkdtempSync(join(tmpdir(), 'coord-peer-test-'));
  });
  afterEach(() => {
    rmSync(scratch, { recursive: true, force: true });
  });

  it('local:<path> → kind=local, resolved=<path>/', async () => {
    const target = join(scratch, 'peer-local');
    const r = await parsePeer(`local:${target}`, scratch);
    expect(r.kind).toBe('local');
    expect(r.resolved).toBe(`${target}/`);
    expect(r.spec).toBe(`local:${target}`);
  });

  it('local: empty path → throws', async () => {
    await expect(parsePeer('local:', scratch)).rejects.toThrowError(
      /local: peer requires a path/
    );
  });

  it('host:path → kind=ssh, resolved=<spec>/', async () => {
    const r = await parsePeer('bob.example.com:/srv/coord', scratch);
    expect(r.kind).toBe('ssh');
    expect(r.resolved).toBe('bob.example.com:/srv/coord/');
  });

  it('bare hostname with no peers.yaml → kind=ssh fallback', async () => {
    const r = await parsePeer('bob.example.com', scratch);
    expect(r.kind).toBe('ssh');
    expect(r.resolved).toBe('bob.example.com:.local/state/coord/');
  });

  it('bare token resolves through peers.yaml alias', async () => {
    mkdirSync(scratch, { recursive: true });
    writeFileSync(
      join(scratch, 'peers.yaml'),
      'bobby: bob.example.com:/srv/coord\n'
    );
    const r = await parsePeer('bobby', scratch);
    expect(r.kind).toBe('alias');
    expect(r.alias).toBe('bobby');
    expect(r.resolved).toBe('bob.example.com:/srv/coord/');
  });

  it('alias targeting local: resolves through to local-style', async () => {
    const target = join(scratch, 'aliased');
    writeFileSync(
      join(scratch, 'peers.yaml'),
      `peer: local:${target}\n`
    );
    const r = await parsePeer('peer', scratch);
    expect(r.kind).toBe('alias');
    expect(r.resolved).toBe(`${target}/`);
  });

  it('alias-miss falls through to bare-hostname ssh fallback', async () => {
    writeFileSync(
      join(scratch, 'peers.yaml'),
      'other: somewhere.example.com\n'
    );
    const r = await parsePeer('not-an-alias', scratch);
    expect(r.kind).toBe('ssh');
    expect(r.resolved).toBe('not-an-alias:.local/state/coord/');
  });

  it('comments and blank lines in peers.yaml are tolerated', async () => {
    writeFileSync(
      join(scratch, 'peers.yaml'),
      '# header\n\nbobby: bob.example.com\n# trailing\n'
    );
    const r = await parsePeer('bobby', scratch);
    expect(r.kind).toBe('alias');
    expect(r.resolved).toBe('bob.example.com:.local/state/coord/');
  });
});
