// tests/unit/ls.test.ts — comprehensive coverage of cmd_ls.

import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { cmdLs, type LsInput } from '../../src/commands/ls.ts';

let scratch: string;
let coordRoot: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-ls-test-'));
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

function writeMsg(
  id: string,
  filename: string,
  fromValue: string,
  body = 'body',
  folder: 'inbox' | 'archive' = 'inbox'
): void {
  writeFileSync(
    join(coordRoot, id, folder, filename),
    `---\nfrom: ${fromValue}\n---\n${body}\n`
  );
}

function baseInput(overrides: Partial<LsInput> = {}): LsInput {
  return {
    recipient: 'bob',
    env: {} as NodeJS.ProcessEnv,
    coordRoot,
    ...overrides,
  };
}

// ─── Basic ──────────────────────────────────────────────────────────────

describe('cmdLs — empty / single / multi', () => {
  it('empty inbox → no matches, header reads "0 messages"', () => {
    setupIdentity('bob');
    const r = cmdLs(baseInput());
    expect(r.matches).toEqual([]);
    expect(r.header).toBe('# 0 messages in inbox');
    expect(r.archive).toBe(false);
  });

  it('one message → header pluralizes singular', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    const r = cmdLs(baseInput());
    expect(r.matches).toEqual(['1714826789010-aaaaaa.md']);
    expect(r.header).toBe('# 1 message in inbox');
  });

  it('two messages → plural', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    writeMsg('bob', '1714826789020-bbbbbb.md', 'alice');
    const r = cmdLs(baseInput());
    expect(r.matches).toHaveLength(2);
    expect(r.header).toBe('# 2 messages in inbox');
  });

  it('listing is chronological (filename) ascending', () => {
    setupIdentity('bob');
    // Insert deliberately out of order.
    writeMsg('bob', '1714826789030-cccccc.md', 'alice');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    writeMsg('bob', '1714826789020-bbbbbb.md', 'alice');
    const r = cmdLs(baseInput());
    expect(r.matches).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789020-bbbbbb.md',
      '1714826789030-cccccc.md',
    ]);
  });
});

// ─── --archive ──────────────────────────────────────────────────────────

describe('cmdLs — --archive', () => {
  it('lists archive/ instead of inbox/, header says "archive"', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice', 'live', 'inbox');
    writeMsg('bob', '1714826789020-bbbbbb.md', 'alice', 'old', 'archive');
    const r = cmdLs(baseInput({ archive: true }));
    expect(r.matches).toEqual(['1714826789020-bbbbbb.md']);
    expect(r.header).toBe('# 1 message in archive');
    expect(r.archive).toBe(true);
  });

  it('empty archive prints "0 messages in archive"', () => {
    setupIdentity('bob');
    const r = cmdLs(baseInput({ archive: true }));
    expect(r.matches).toEqual([]);
    expect(r.header).toBe('# 0 messages in archive');
  });
});

// ─── --since ────────────────────────────────────────────────────────────

describe('cmdLs — --since', () => {
  it('filters by filename timestamp (>= cutoff)', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    writeMsg('bob', '1714826789020-bbbbbb.md', 'alice');
    writeMsg('bob', '1714826789030-cccccc.md', 'alice');
    const r = cmdLs(baseInput({ since: 1714826789020 }));
    expect(r.matches).toEqual([
      '1714826789020-bbbbbb.md',
      '1714826789030-cccccc.md',
    ]);
  });

  it('since=0 includes everything', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    expect(cmdLs(baseInput({ since: 0 })).matches).toHaveLength(1);
  });

  it('since strictly greater than every ts → no matches', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    expect(cmdLs(baseInput({ since: 9999999999999 })).matches).toEqual([]);
  });
});

// ─── --from ─────────────────────────────────────────────────────────────

describe('cmdLs — --from', () => {
  it('filters by frontmatter from: key', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    writeMsg('bob', '1714826789020-bbbbbb.md', 'carol');
    writeMsg('bob', '1714826789030-cccccc.md', 'alice');
    const r = cmdLs(baseInput({ fromFilter: 'alice' }));
    expect(r.matches).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789030-cccccc.md',
    ]);
  });

  it('no match → empty list', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    expect(cmdLs(baseInput({ fromFilter: 'ghost' })).matches).toEqual([]);
  });

  it('files with malformed frontmatter are silently excluded', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    // Malformed: no fences, body only.
    writeFileSync(
      join(coordRoot, 'bob', 'inbox', '1714826789020-bbbbbb.md'),
      'just body, no frontmatter'
    );
    const r = cmdLs(baseInput({ fromFilter: 'alice' }));
    expect(r.matches).toEqual(['1714826789010-aaaaaa.md']);
  });

  it('combines with --since (intersection)', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    writeMsg('bob', '1714826789020-bbbbbb.md', 'alice');
    writeMsg('bob', '1714826789030-cccccc.md', 'carol');
    const r = cmdLs(baseInput({ since: 1714826789020, fromFilter: 'alice' }));
    expect(r.matches).toEqual(['1714826789020-bbbbbb.md']);
  });
});

// ─── Identity resolution ────────────────────────────────────────────────

describe('cmdLs — identity resolution', () => {
  it('uses positional recipient (env ignored)', () => {
    setupIdentity('alice');
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    const r = cmdLs(
      baseInput({
        recipient: 'bob',
        env: { COORD_IDENTITY: 'alice' } as NodeJS.ProcessEnv,
      })
    );
    expect(r.matches).toHaveLength(1);
  });

  it('falls back to COORD_IDENTITY when no positional', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    const r = cmdLs(
      baseInput({
        recipient: undefined,
        env: { COORD_IDENTITY: 'bob' } as NodeJS.ProcessEnv,
      })
    );
    expect(r.matches).toHaveLength(1);
  });

  it('errors when neither positional nor env identity is set', () => {
    expect(() =>
      cmdLs(baseInput({ recipient: undefined, env: {} as NodeJS.ProcessEnv }))
    ).toThrowError(/COORD_IDENTITY/);
  });

  it('errors with mkdir hint when identity folder is missing', () => {
    expect(() => cmdLs(baseInput({ recipient: 'ghost' }))).toThrowError(
      /(agent|identity) folder missing/
    );
  });
});

// ─── Filename grammar gating ────────────────────────────────────────────

describe('cmdLs — non-grammar files in the inbox', () => {
  it('non-.md files (README, dotfiles, attachment sidecars) are still skipped', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    writeFileSync(join(coordRoot, 'bob', 'inbox', 'README'), 'stray');
    writeFileSync(join(coordRoot, 'bob', 'inbox', '.hidden.md'), 'dotfile');
    // Attachment sidecar of a canonical .md — reserved for the canonical.
    writeFileSync(
      join(coordRoot, 'bob', 'inbox', '1714826789010-aaaaaa.subject.md'),
      'sidecar'
    );
    const r = cmdLs(baseInput());
    expect(r.matches).toEqual(['1714826789010-aaaaaa.md']);
  });

  // Task #128: off-format `.md` files that landed in the inbox are
  // now surfaced as "outside" messages rather than silently dropped.
  it('outside `.md` files are listed alongside canonical messages', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    writeFileSync(join(coordRoot, 'bob', 'inbox', 'notes.md'), 'stray');
    writeFileSync(
      join(coordRoot, 'bob', 'inbox', '1714826789010-myobie-aaaaaa.md'),
      'legacy-shape'
    );
    const r = cmdLs(baseInput());
    // Sort order is by basename (alphabetical) — that's what readdir
    // gives us, and outside filenames have no LAYOUT prefix to sort by.
    expect(r.matches).toContain('1714826789010-aaaaaa.md');
    expect(r.matches).toContain('notes.md');
    expect(r.matches).toContain('1714826789010-myobie-aaaaaa.md');
    expect(r.matches).toHaveLength(3);
  });

  it('withMeta stamps outside .md files with from="outside" and empty projections', () => {
    setupIdentity('bob');
    writeFileSync(join(coordRoot, 'bob', 'inbox', 'hello.md'), 'hi\n');
    const r = cmdLs(baseInput({ withMeta: true }));
    expect(r.items).toBeDefined();
    const outside = r.items!.find((i) => i.filename === 'hello.md');
    expect(outside).toBeDefined();
    expect(outside!.from).toBe('outside');
    expect(outside!.subject).toBeNull();
    expect(outside!.inReplyTo).toBeNull();
    expect(outside!.tags).toEqual([]);
    expect(outside!.priority).toBeNull();
    // ts derived from file mtime — non-zero on any real filesystem.
    expect(outside!.ts).toBeGreaterThan(0);
  });
});

// ─── Missing inbox directory ────────────────────────────────────────────

describe('cmdLs — missing inbox dir', () => {
  it('returns 0 matches without throwing if the inbox dir disappears', () => {
    setupIdentity('bob');
    rmSync(join(coordRoot, 'bob', 'inbox'), { recursive: true });
    // Cross-identity reads are lenient: as long as ONE of
    // inbox/archive exists, the resolver succeeds and the missing
    // folder is treated as empty by existsSync below. Auto-create
    // is not triggered (cross-identity read shouldn't materialize
    // folders for an identity we're observing).
    const r = cmdLs(baseInput());
    expect(r.matches).toEqual([]);
    // bob/inbox was NOT recreated by the read.
    expect(existsSync(join(coordRoot, 'bob', 'inbox'))).toBe(false);
  });

  it('after recreating only inbox, listing is empty', () => {
    setupIdentity('bob');
    const r = cmdLs(baseInput());
    expect(r.matches).toEqual([]);
  });
});

// ─── withMeta (JSON output) ─────────────────────────────────────────────

describe('cmdLs — withMeta', () => {
  it('omits items by default', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    const r = cmdLs(baseInput());
    expect(r.items).toBeUndefined();
  });

  it('populates items in same order as matches', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    writeMsg('bob', '1714826789020-bbbbbb.md', 'carol');
    const r = cmdLs(baseInput({ withMeta: true }));
    expect(r.items).toBeDefined();
    expect(r.items!.length).toBe(r.matches.length);
    expect(r.items!.map((i) => i.filename)).toEqual(r.matches);
  });

  it('every item carries filename + ts derived from the prefix', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    const r = cmdLs(baseInput({ withMeta: true }));
    expect(r.items![0]).toMatchObject({
      filename: '1714826789010-aaaaaa.md',
      ts: 1714826789010,
      from: 'alice',
    });
  });

  it('absent frontmatter fields surface as null (not undefined or missing)', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    const item = cmdLs(baseInput({ withMeta: true })).items![0]!;
    expect(item.subject).toBeNull();
    expect(item.inReplyTo).toBeNull();
    expect(item.priority).toBeNull();
    expect(item.tags).toEqual([]);
  });

  it('parses subject + in-reply-to + priority when present', () => {
    setupIdentity('bob');
    writeFileSync(
      join(coordRoot, 'bob', 'inbox', '1714826789010-aaaaaa.md'),
      '---\n' +
        'from: alice\n' +
        'subject: deploy question\n' +
        'in-reply-to: 1714826789000-zzzzzz.md\n' +
        'priority: high\n' +
        '---\nbody\n'
    );
    const item = cmdLs(baseInput({ withMeta: true })).items![0]!;
    expect(item.subject).toBe('deploy question');
    expect(item.inReplyTo).toBe('1714826789000-zzzzzz.md');
    expect(item.priority).toBe('high');
  });

  it('parses tags written as a YAML-list scalar', () => {
    setupIdentity('bob');
    writeFileSync(
      join(coordRoot, 'bob', 'inbox', '1714826789010-aaaaaa.md'),
      '---\nfrom: alice\ntags: [a, b, c]\n---\nbody\n'
    );
    const item = cmdLs(baseInput({ withMeta: true })).items![0]!;
    expect(item.tags).toEqual(['a', 'b', 'c']);
  });

  it('files with no frontmatter still appear with from=null', () => {
    setupIdentity('bob');
    writeFileSync(
      join(coordRoot, 'bob', 'inbox', '1714826789010-aaaaaa.md'),
      'no fence here, just words\n'
    );
    const item = cmdLs(baseInput({ withMeta: true })).items![0]!;
    expect(item.from).toBeNull();
    expect(item.filename).toBe('1714826789010-aaaaaa.md');
    expect(item.ts).toBe(1714826789010);
  });

  it('empty inbox → items is []', () => {
    setupIdentity('bob');
    const r = cmdLs(baseInput({ withMeta: true }));
    expect(r.items).toEqual([]);
  });

  it('composes with --since', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    writeMsg('bob', '1714826789020-bbbbbb.md', 'alice');
    writeMsg('bob', '1714826789030-cccccc.md', 'alice');
    const r = cmdLs(baseInput({ withMeta: true, since: 1714826789020 }));
    expect(r.matches).toEqual([
      '1714826789020-bbbbbb.md',
      '1714826789030-cccccc.md',
    ]);
    expect(r.items!.map((i) => i.filename)).toEqual(r.matches);
  });

  it('composes with --from', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    writeMsg('bob', '1714826789020-bbbbbb.md', 'carol');
    const r = cmdLs(baseInput({ withMeta: true, fromFilter: 'alice' }));
    expect(r.matches).toEqual(['1714826789010-aaaaaa.md']);
    expect(r.items![0]!.from).toBe('alice');
  });

  it('composes with --archive', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice', 'archived', 'archive');
    const r = cmdLs(baseInput({ withMeta: true, archive: true }));
    expect(r.archive).toBe(true);
    expect(r.items!.map((i) => i.filename)).toEqual(['1714826789010-aaaaaa.md']);
  });
});

// ─── --json CLI flag ────────────────────────────────────────────────────

describe('cmdLsCli — --json output', () => {
  it('returns valid JSON parseable by JSON.parse', async () => {
    const { cmdLsCli } = await import('../../src/commands/ls.ts');
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    let stdout = '';
    cmdLsCli(['bob', '--json'], {
      env: {} as NodeJS.ProcessEnv,
      coordRoot,
      coordConfig: '',
      stdout: (s) => {
        stdout += s;
      },
      stderr: () => {},
      readStdin: async () => Buffer.from(''),
    });
    const parsed: unknown = JSON.parse(stdout);
    expect(Array.isArray(parsed)).toBe(true);
    expect(parsed).toHaveLength(1);
  });

  it('empty inbox → "[]"', async () => {
    const { cmdLsCli } = await import('../../src/commands/ls.ts');
    setupIdentity('bob');
    let stdout = '';
    cmdLsCli(['bob', '--json'], {
      env: {} as NodeJS.ProcessEnv,
      coordRoot,
      coordConfig: '',
      stdout: (s) => {
        stdout += s;
      },
      stderr: () => {},
      readStdin: async () => Buffer.from(''),
    });
    expect(stdout.trim()).toBe('[]');
  });

  it('--count and --json together → error', async () => {
    const { cmdLsCli } = await import('../../src/commands/ls.ts');
    setupIdentity('bob');
    expect(() =>
      cmdLsCli(['bob', '--count', '--json'], {
        env: {} as NodeJS.ProcessEnv,
        coordRoot,
        coordConfig: '',
        stdout: () => {},
        stderr: () => {},
        readStdin: async () => Buffer.from(''),
      })
    ).toThrowError(/--count and --json are mutually exclusive/);
  });
});

// ─── --orphans (issue #8) ──────────────────────────────────────────────────

function writeAttachment(
  id: string,
  filename: string,
  body: string,
  folder: 'inbox' | 'archive' = 'inbox'
): void {
  writeFileSync(join(coordRoot, id, folder, filename), body);
}

describe('cmdLs — --orphans', () => {
  it('inbox sibling with no matching .md → reported as orphan', () => {
    setupIdentity('bob');
    writeAttachment(
      'bob',
      '1714826789010-aaaaaa.options.json',
      '{"k":1}'
    );
    const r = cmdLs(baseInput({ orphans: true }));
    expect(r.matches).toEqual(['1714826789010-aaaaaa.options.json']);
    expect(r.header).toContain('1 orphan attachment');
  });

  it('sibling WITH matching .md → not an orphan', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    writeAttachment(
      'bob',
      '1714826789010-aaaaaa.options.json',
      '{"k":1}'
    );
    const r = cmdLs(baseInput({ orphans: true }));
    expect(r.matches).toEqual([]);
    expect(r.header).toContain('0 orphan attachments');
  });

  it('.md files themselves are never orphans', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    const r = cmdLs(baseInput({ orphans: true }));
    expect(r.matches).toEqual([]);
  });

  it('random files (no LAYOUT prefix) are ignored, not reported', () => {
    setupIdentity('bob');
    writeAttachment('bob', 'README', 'x');
    writeAttachment('bob', '.DS_Store', 'x');
    const r = cmdLs(baseInput({ orphans: true }));
    expect(r.matches).toEqual([]);
  });

  it('mixed: archived .md + orphan siblings remaining in inbox', () => {
    setupIdentity('bob');
    // .md was archived without --with-attachments, leaving two
    // orphans behind in inbox. This is the issue's exact scenario.
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice', 'body', 'archive');
    writeAttachment(
      'bob',
      '1714826789010-aaaaaa.options.json',
      '{"k":1}'
    );
    writeAttachment(
      'bob',
      '1714826789010-aaaaaa.schema.json',
      '{}'
    );
    const r = cmdLs(baseInput({ orphans: true }));
    expect(r.matches).toEqual([
      '1714826789010-aaaaaa.options.json',
      '1714826789010-aaaaaa.schema.json',
    ]);
    expect(r.header).toContain('2 orphan attachments');
  });

  it('--archive scopes orphan scan to archive/', () => {
    setupIdentity('bob');
    // Orphan only in archive (someone manually moved a sibling but
    // not the .md — unusual, but the detector covers it).
    writeAttachment(
      'bob',
      '1714826789010-aaaaaa.options.json',
      '{}',
      'archive'
    );
    const r = cmdLs(baseInput({ orphans: true, archive: true }));
    expect(r.matches).toEqual(['1714826789010-aaaaaa.options.json']);
  });

  it('--since filters by the prefix ts', () => {
    setupIdentity('bob');
    writeAttachment(
      'bob',
      '1714826789010-aaaaaa.options.json',
      'old'
    );
    writeAttachment(
      'bob',
      '1714826790000-bbbbbb.options.json',
      'new'
    );
    const r = cmdLs(baseInput({ orphans: true, since: 1714826789500 }));
    expect(r.matches).toEqual(['1714826790000-bbbbbb.options.json']);
  });

  it('CLI: --orphans + --json emits [{filename, ts}, ...]', async () => {
    const { cmdLsCli } = await import('../../src/commands/ls.ts');
    setupIdentity('bob');
    writeAttachment(
      'bob',
      '1714826789010-aaaaaa.options.json',
      '{}'
    );
    let stdout = '';
    cmdLsCli(['bob', '--orphans', '--json'], {
      env: {} as NodeJS.ProcessEnv,
      coordRoot,
      coordConfig: '',
      stdout: (s) => {
        stdout += s;
      },
      stderr: () => {},
      readStdin: async () => Buffer.from(''),
    });
    expect(JSON.parse(stdout)).toEqual([
      { filename: '1714826789010-aaaaaa.options.json', ts: 1714826789010 },
    ]);
  });

  it('CLI: --orphans + --from is rejected', async () => {
    const { cmdLsCli } = await import('../../src/commands/ls.ts');
    setupIdentity('bob');
    expect(() =>
      cmdLsCli(['bob', '--orphans', '--from', 'alice'], {
        env: {} as NodeJS.ProcessEnv,
        coordRoot,
        coordConfig: '',
        stdout: () => {},
        stderr: () => {},
        readStdin: async () => Buffer.from(''),
      })
    ).toThrowError(/--orphans and --from/);
  });

  it('CLI: --orphans --count just prints the count', async () => {
    const { cmdLsCli } = await import('../../src/commands/ls.ts');
    setupIdentity('bob');
    writeAttachment(
      'bob',
      '1714826789010-aaaaaa.options.json',
      'x'
    );
    writeAttachment(
      'bob',
      '1714826789020-bbbbbb.schema.json',
      'y'
    );
    let stdout = '';
    cmdLsCli(['bob', '--orphans', '--count'], {
      env: {} as NodeJS.ProcessEnv,
      coordRoot,
      coordConfig: '',
      stdout: (s) => {
        stdout += s;
      },
      stderr: () => {},
      readStdin: async () => Buffer.from(''),
    });
    expect(stdout.trim()).toBe('2');
  });
});
