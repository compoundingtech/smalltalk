// tests/unit/resource.test.ts — resource add/ls/read/rm core behavior.
//
// brief-009 item 5: <identity>/resources/<filename>.md with `url:` in
// frontmatter and an optional description body.

import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  cmdResourceAdd,
  cmdResourceLs,
  cmdResourceRead,
  cmdResourceRemove,
  listResourceRecords,
} from '../../src/commands/resource.ts';
import {
  InvalidFilenameError,
  InvalidResourceUrlError,
  ResourceNotFoundError,
} from '../../src/errors.ts';

let scratch: string;
let coordRoot: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-resource-test-'));
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

// ─── add ───────────────────────────────────────────────────────────────

describe('cmdResourceAdd', () => {
  it('writes a LAYOUT-004 filename with url in frontmatter', () => {
    setupIdentity('alice');
    const r = cmdResourceAdd({
      url: 'https://example.com/foo',
      env: envFor('alice'),
      coordRoot,
    });
    expect(r.identity).toBe('alice');
    expect(/^[0-9]{13}-[0-9a-z]{6}\.md$/.test(r.filename)).toBe(true);
    const text = readFileSync(r.path, 'utf8');
    expect(text).toContain('---\n');
    // url contains `:` so emitFrontmatter wraps it in double quotes.
    expect(text).toContain('url: "https://example.com/foo"');
  });

  it('lazily creates resources/ on first add', () => {
    setupIdentity('alice');
    expect(existsSync(join(coordRoot, 'alice', 'resources'))).toBe(false);
    cmdResourceAdd({
      url: 'https://example.com',
      env: envFor('alice'),
      coordRoot,
    });
    expect(existsSync(join(coordRoot, 'alice', 'resources'))).toBe(true);
  });

  it('persists title + tags + body in the on-disk file', () => {
    setupIdentity('alice');
    const r = cmdResourceAdd({
      url: 'https://github.com/myobie/smalltalk/pull/17',
      title: 'PR #17 — remove tasks',
      tags: ['pr', 'brief-009'],
      body: 'first slim-down PR\n',
      env: envFor('alice'),
      coordRoot,
    });
    const text = readFileSync(r.path, 'utf8');
    // title contains `:` (none here actually) and `#` — yamlQuote
    // double-quotes anything that isn't safe-bareword.
    expect(text).toMatch(/title: ["']?PR #17/);
    expect(text).toContain('pr');
    expect(text).toContain('brief-009');
    expect(text).toContain('first slim-down PR');
  });

  it('rejects URLs without a scheme', () => {
    setupIdentity('alice');
    expect(() =>
      cmdResourceAdd({
        url: 'example.com',
        env: envFor('alice'),
        coordRoot,
      })
    ).toThrow(InvalidResourceUrlError);
  });

  it('accepts a `pty://` scheme (convention for pty sessions)', () => {
    setupIdentity('alice');
    expect(() =>
      cmdResourceAdd({
        url: 'pty://my-session',
        env: envFor('alice'),
        coordRoot,
      })
    ).not.toThrow();
  });

  it('rejects empty URL', () => {
    setupIdentity('alice');
    expect(() =>
      cmdResourceAdd({
        url: '',
        env: envFor('alice'),
        coordRoot,
      })
    ).toThrow(InvalidResourceUrlError);
  });

  it('comma-splits a comma-separated string tag arg', () => {
    setupIdentity('alice');
    const r = cmdResourceAdd({
      url: 'https://example.com',
      tags: 'a,b,c',
      env: envFor('alice'),
      coordRoot,
    });
    const items = listResourceRecords('alice', coordRoot);
    const rec = items.find((it) => it.filename === r.filename)!;
    expect(rec.tags).toEqual(['a', 'b', 'c']);
  });
});

// ─── ls ────────────────────────────────────────────────────────────────

describe('cmdResourceLs', () => {
  it('empty resources/ → empty matches', () => {
    setupIdentity('alice');
    const r = cmdResourceLs({ env: envFor('alice'), coordRoot });
    expect(r.matches).toEqual([]);
  });

  it('lists only valid-grammar files', () => {
    setupIdentity('alice');
    const r1 = cmdResourceAdd({
      url: 'https://example.com/1',
      env: envFor('alice'),
      coordRoot,
    });
    const r2 = cmdResourceAdd({
      url: 'https://example.com/2',
      env: envFor('alice'),
      coordRoot,
    });
    // Noise file that shouldn't match the grammar
    writeFileSync(join(coordRoot, 'alice', 'resources', 'README'), 'noise');
    writeFileSync(
      join(coordRoot, 'alice', 'resources', 'not-a-resource.md'),
      'noise'
    );
    const r = cmdResourceLs({ env: envFor('alice'), coordRoot });
    expect(r.matches).toContain(r1.filename);
    expect(r.matches).toContain(r2.filename);
    expect(r.matches).not.toContain('README');
    expect(r.matches).not.toContain('not-a-resource.md');
  });

  it('lists a peer\'s resources when identity passed explicitly', () => {
    setupIdentity('alice');
    setupIdentity('bob');
    const r = cmdResourceAdd({
      url: 'https://example.com',
      env: envFor('bob'),
      coordRoot,
    });
    const ls = cmdResourceLs({
      identity: 'bob',
      env: envFor('alice'),
      coordRoot,
    });
    expect(ls.identity).toBe('bob');
    expect(ls.matches).toContain(r.filename);
  });

  it('missing resources/ folder yields empty matches, not throw', () => {
    setupIdentity('alice');
    const r = cmdResourceLs({ env: envFor('alice'), coordRoot });
    expect(r.matches).toEqual([]);
  });
});

// ─── read ──────────────────────────────────────────────────────────────

describe('cmdResourceRead', () => {
  it('round-trips url + title + tags + body', () => {
    setupIdentity('alice');
    const added = cmdResourceAdd({
      url: 'https://example.com/foo',
      title: 'foo',
      tags: ['x', 'y'],
      body: 'a description\n',
      env: envFor('alice'),
      coordRoot,
    });
    const r = cmdResourceRead({
      filename: added.filename,
      env: envFor('alice'),
      coordRoot,
    });
    expect(r.identity).toBe('alice');
    expect(r.record.url).toBe('https://example.com/foo');
    expect(r.record.title).toBe('foo');
    expect(r.record.tags).toEqual(['x', 'y']);
    expect(r.record.body).toContain('a description');
  });

  it('reads a peer\'s resource when identity passed explicitly', () => {
    setupIdentity('alice');
    setupIdentity('bob');
    const added = cmdResourceAdd({
      url: 'https://example.com',
      env: envFor('bob'),
      coordRoot,
    });
    const r = cmdResourceRead({
      identity: 'bob',
      filename: added.filename,
      env: envFor('alice'),
      coordRoot,
    });
    expect(r.identity).toBe('bob');
    expect(r.record.url).toBe('https://example.com');
  });

  it('throws ResourceNotFoundError when filename does not exist', () => {
    setupIdentity('alice');
    expect(() =>
      cmdResourceRead({
        filename: '1714826789010-aaaaaa.md',
        env: envFor('alice'),
        coordRoot,
      })
    ).toThrow(ResourceNotFoundError);
  });

  it('throws InvalidFilenameError on bad filename grammar', () => {
    setupIdentity('alice');
    expect(() =>
      cmdResourceRead({
        filename: 'not-a-grammar',
        env: envFor('alice'),
        coordRoot,
      })
    ).toThrow(InvalidFilenameError);
  });

  it('treats missing title and tags as null/[] (not undefined)', () => {
    setupIdentity('alice');
    const added = cmdResourceAdd({
      url: 'https://example.com',
      env: envFor('alice'),
      coordRoot,
    });
    const r = cmdResourceRead({
      filename: added.filename,
      env: envFor('alice'),
      coordRoot,
    });
    expect(r.record.title).toBeNull();
    expect(r.record.tags).toEqual([]);
  });
});

// ─── rm ────────────────────────────────────────────────────────────────

describe('cmdResourceRemove', () => {
  it('deletes the file when it exists; subsequent read throws', () => {
    setupIdentity('alice');
    const added = cmdResourceAdd({
      url: 'https://example.com',
      env: envFor('alice'),
      coordRoot,
    });
    cmdResourceRemove({
      filename: added.filename,
      env: envFor('alice'),
      coordRoot,
    });
    expect(existsSync(added.path)).toBe(false);
    expect(() =>
      cmdResourceRead({
        filename: added.filename,
        env: envFor('alice'),
        coordRoot,
      })
    ).toThrow(ResourceNotFoundError);
  });

  it('throws ResourceNotFoundError when the filename is unknown', () => {
    setupIdentity('alice');
    expect(() =>
      cmdResourceRemove({
        filename: '1714826789010-aaaaaa.md',
        env: envFor('alice'),
        coordRoot,
      })
    ).toThrow(ResourceNotFoundError);
  });

  it('rm without explicit identity only touches the OWNER\'s own resources', () => {
    // Alice tries to remove a resource owned by bob — should miss alice's
    // (empty) and not touch bob's. The single-writer rule: rm operates
    // on the resolved (own) identity, never on a peer's tree.
    setupIdentity('alice');
    setupIdentity('bob');
    const bobAdded = cmdResourceAdd({
      url: 'https://example.com',
      env: envFor('bob'),
      coordRoot,
    });
    expect(() =>
      cmdResourceRemove({
        filename: bobAdded.filename,
        env: envFor('alice'),
        coordRoot,
      })
    ).toThrow(ResourceNotFoundError);
    // Bob's file is intact.
    expect(existsSync(bobAdded.path)).toBe(true);
  });
});

// ─── listResourceRecords (helper used by SDK + MCP) ────────────────────

describe('listResourceRecords', () => {
  it('returns parsed records sorted by filename', () => {
    setupIdentity('alice');
    cmdResourceAdd({
      url: 'https://example.com/a',
      env: envFor('alice'),
      coordRoot,
    });
    cmdResourceAdd({
      url: 'https://example.com/b',
      env: envFor('alice'),
      coordRoot,
    });
    const items = listResourceRecords('alice', coordRoot);
    expect(items).toHaveLength(2);
    // sorted by filename (which is timestamp-prefixed)
    expect(items[0]!.filename < items[1]!.filename).toBe(true);
  });

  it('missing folder → [] (no throw)', () => {
    expect(listResourceRecords('nobody', coordRoot)).toEqual([]);
  });
});
