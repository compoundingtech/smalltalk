// tests/unit/read.test.ts — comprehensive coverage of cmd_read.

import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  buildReadJsonShape,
  cmdRead,
  cmdReadCli,
  splitReadPositionals,
  type ReadInput,
} from '../../src/commands/read.ts';
import type { CliContext } from '../../src/cli-context.ts';

let scratch: string;
let coordRoot: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-read-test-'));
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

function writeFile(
  id: string,
  filename: string,
  content: string,
  folder: 'inbox' | 'archive' = 'inbox'
): void {
  writeFileSync(join(coordRoot, id, folder, filename), content);
}

function baseInput(overrides: Partial<ReadInput> = {}): ReadInput {
  return {
    recipient: 'bob',
    filename: '1714826789010-aaaaaa.md',
    env: {} as NodeJS.ProcessEnv,
    coordRoot,
    ...overrides,
  };
}

// ─── Happy paths ────────────────────────────────────────────────────────

describe('cmdRead — formatted mode', () => {
  it('inbox file: header derives to/ts; body separated', () => {
    setupIdentity('bob');
    writeFile(
      'bob',
      '1714826789010-aaaaaa.md',
      '---\nfrom: alice\nsubject: hi\n---\nthe body\n'
    );
    const r = cmdRead(baseInput());
    expect(r.label).toBe('inbox');
    expect(r.untyped).toBe(false);
    expect(r.header).toContain('# inbox/1714826789010-aaaaaa.md');
    expect(r.header).toContain('to:          bob  (derived from path)');
    expect(r.header).toContain(
      'ts:          1714826789010  (derived from filename)'
    );
    // Header padding: label padded to 12 + 1 space = 13-char prefix
    // before the value, matching the bash printf format.
    expect(r.header).toContain('from:        alice'); // 8 spaces
    expect(r.header).toContain('subject:     hi'); // 5 spaces
    expect(r.body).toBe('the body\n');
  });

  it('archive file (with --archive) returns archive header label', () => {
    setupIdentity('bob');
    writeFile(
      'bob',
      '1714826789010-aaaaaa.md',
      '---\nfrom: alice\n---\narchived body\n',
      'archive'
    );
    const r = cmdRead(baseInput({ fromArchive: true }));
    expect(r.label).toBe('archive');
    expect(r.header).toContain('# archive/1714826789010-aaaaaa.md');
    expect(r.body).toBe('archived body\n');
  });

  it('auto-fallback: not in inbox, IS in archive → reads archive', () => {
    setupIdentity('bob');
    writeFile(
      'bob',
      '1714826789010-aaaaaa.md',
      '---\nfrom: alice\n---\narchived\n',
      'archive'
    );
    const r = cmdRead(baseInput());
    expect(r.label).toBe('archive');
  });

  it('inbox preferred when --archive not set and file in both', () => {
    setupIdentity('bob');
    writeFile(
      'bob',
      '1714826789010-aaaaaa.md',
      '---\nfrom: alice\n---\ninbox-version\n'
    );
    writeFile(
      'bob',
      '1714826789010-aaaaaa.md',
      '---\nfrom: alice\n---\narchive-version\n',
      'archive'
    );
    const r = cmdRead(baseInput());
    expect(r.label).toBe('inbox');
    expect(r.body).toBe('inbox-version\n');
  });

  it('--archive prefers archive even when inbox copy exists', () => {
    setupIdentity('bob');
    writeFile(
      'bob',
      '1714826789010-aaaaaa.md',
      '---\nfrom: alice\n---\ninbox-version\n'
    );
    writeFile(
      'bob',
      '1714826789010-aaaaaa.md',
      '---\nfrom: alice\n---\narchive-version\n',
      'archive'
    );
    const r = cmdRead(baseInput({ fromArchive: true }));
    expect(r.label).toBe('archive');
    expect(r.body).toBe('archive-version\n');
  });

  it('omits empty frontmatter rows from the header', () => {
    setupIdentity('bob');
    writeFile('bob', '1714826789010-aaaaaa.md', '---\nfrom: alice\n---\nbody\n');
    const r = cmdRead(baseInput());
    expect(r.header).not.toContain('subject:');
    expect(r.header).not.toContain('in-reply-to:');
    expect(r.header).not.toContain('tags:');
    expect(r.header).not.toContain('priority:');
  });

  it('shows in-reply-to when present', () => {
    setupIdentity('bob');
    writeFile(
      'bob',
      '1714826789010-aaaaaa.md',
      '---\nfrom: alice\nin-reply-to: 1714826789000-zzzzzz.md\n---\nbody\n'
    );
    const r = cmdRead(baseInput());
    expect(r.header).toContain('in-reply-to: 1714826789000-zzzzzz.md');
  });

  it('shows tags when present', () => {
    setupIdentity('bob');
    writeFile(
      'bob',
      '1714826789010-aaaaaa.md',
      '---\nfrom: alice\ntags: [a, b]\n---\nbody\n'
    );
    const r = cmdRead(baseInput());
    expect(r.header).toContain('tags:        [a, b]'); // 8 spaces
  });
});

// ─── --raw ──────────────────────────────────────────────────────────────

describe('cmdRead — --raw', () => {
  it('returns the file verbatim and empty header', () => {
    setupIdentity('bob');
    const text = '---\nfrom: alice\nsubject: hi\n---\nthe body\n';
    writeFile('bob', '1714826789010-aaaaaa.md', text);
    const r = cmdRead(baseInput({ raw: true }));
    expect(r.body).toBe(text);
    expect(r.header).toBe('');
    expect(r.untyped).toBe(false);
  });

  it('--raw on a no-frontmatter file dumps body verbatim', () => {
    setupIdentity('bob');
    const text = 'just body, no frontmatter\n';
    writeFile('bob', '1714826789010-aaaaaa.md', text);
    const r = cmdRead(baseInput({ raw: true }));
    expect(r.body).toBe(text);
  });
});

// ─── Untyped (no frontmatter) ───────────────────────────────────────────

describe('cmdRead — files without frontmatter', () => {
  it('formatted mode marks "(untyped: no frontmatter)" and prints body verbatim', () => {
    setupIdentity('bob');
    writeFile('bob', '1714826789010-aaaaaa.md', 'just text\n');
    const r = cmdRead(baseInput());
    expect(r.untyped).toBe(true);
    expect(r.header).toContain('(untyped: no frontmatter)');
    expect(r.body).toBe('just text\n');
  });

  it('unterminated fence is treated as untyped (permissive read)', () => {
    setupIdentity('bob');
    writeFile(
      'bob',
      '1714826789010-aaaaaa.md',
      '---\nfrom: alice\nno close fence\nbody\n'
    );
    const r = cmdRead(baseInput());
    expect(r.untyped).toBe(true);
  });

  it('empty file: untyped, body is empty', () => {
    setupIdentity('bob');
    writeFile('bob', '1714826789010-aaaaaa.md', '');
    const r = cmdRead(baseInput());
    expect(r.untyped).toBe(true);
    expect(r.body).toBe('');
  });
});

// ─── Outside .md (task #128) ───────────────────────────────────────────

describe('cmdRead — outside .md files', () => {
  it('reads an off-format `.md` as untyped with a "(outside)" header marker', () => {
    setupIdentity('bob');
    writeFile('bob', 'notes.md', 'hand-dropped file\n');
    const r = cmdRead(baseInput({ filename: 'notes.md' }));
    expect(r.untyped).toBe(true);
    expect(r.label).toBe('inbox');
    expect(r.header).toContain('# inbox/notes.md');
    expect(r.header).toContain('(outside: non-canonical filename)');
    expect(r.body).toBe('hand-dropped file\n');
    expect(r.fm).toEqual({});
  });

  it('an outside `.md` with frontmatter is still treated as outside (fm not projected)', () => {
    setupIdentity('bob');
    writeFile(
      'bob',
      'nope.md',
      '---\nfrom: alice\nsubject: forged?\n---\nbody\n'
    );
    const r = cmdRead(baseInput({ filename: 'nope.md' }));
    expect(r.untyped).toBe(true);
    expect(r.fm).toEqual({});
  });

  it('rejects unsafe basenames (path traversal, dotfile)', () => {
    setupIdentity('bob');
    expect(() =>
      cmdRead(baseInput({ filename: '../escape.md' }))
    ).toThrowError(/invalid filename/);
    expect(() =>
      cmdRead(baseInput({ filename: '.hidden.md' }))
    ).toThrowError(/invalid filename/);
  });
});

// ─── Errors ─────────────────────────────────────────────────────────────

describe('cmdRead — errors', () => {
  it('not found anywhere → error mentions both folders', () => {
    setupIdentity('bob');
    expect(() => cmdRead(baseInput())).toThrowError(
      /not found in inbox or archive/
    );
  });

  it('invalid filename grammar → error before filesystem hit', () => {
    setupIdentity('bob');
    expect(() =>
      cmdRead(baseInput({ filename: 'garbage' }))
    ).toThrowError(/invalid filename/);
  });

  it('empty filename → error', () => {
    setupIdentity('bob');
    expect(() =>
      cmdRead(baseInput({ filename: '' }))
    ).toThrowError(/required/);
  });

  it('unknown identity → mkdir hint', () => {
    expect(() =>
      cmdRead(baseInput({ recipient: 'ghost' }))
    ).toThrowError(/(agent|identity) folder missing/);
  });

  it('no recipient + no COORD_IDENTITY → identity-required error', () => {
    expect(() =>
      cmdRead(baseInput({ recipient: undefined }))
    ).toThrowError(/COORD_IDENTITY/);
  });
});

// ─── Identity resolution ────────────────────────────────────────────────

describe('cmdRead — identity resolution', () => {
  it('uses positional recipient over env', () => {
    setupIdentity('bob');
    setupIdentity('alice');
    writeFile('bob', '1714826789010-aaaaaa.md', '---\nfrom: a\n---\nb\n');
    const r = cmdRead(
      baseInput({
        recipient: 'bob',
        env: { COORD_IDENTITY: 'alice' } as NodeJS.ProcessEnv,
      })
    );
    expect(r.label).toBe('inbox');
  });

  it('falls back to COORD_IDENTITY when no positional', () => {
    setupIdentity('bob');
    writeFile('bob', '1714826789010-aaaaaa.md', '---\nfrom: a\n---\nb\n');
    const r = cmdRead(
      baseInput({
        recipient: undefined,
        env: { COORD_IDENTITY: 'bob' } as NodeJS.ProcessEnv,
      })
    );
    expect(r.label).toBe('inbox');
  });
});

// ─── Positional disambiguation ──────────────────────────────────────────

describe('splitReadPositionals', () => {
  it('zero args → both undefined', () => {
    expect(splitReadPositionals([])).toEqual({});
  });

  it('one arg matching filename grammar → filename, recipient undefined', () => {
    expect(splitReadPositionals(['1714826789010-aaaaaa.md'])).toEqual({
      filename: '1714826789010-aaaaaa.md',
    });
  });

  it('one arg NOT matching grammar → recipient, filename undefined', () => {
    expect(splitReadPositionals(['bob'])).toEqual({ recipient: 'bob' });
  });

  // brief-017a bug 2: a non-grammar filename with .md suffix should
  // still parse as a filename so the core's InvalidFilenameError
  // fires instead of the misleading "<filename> required" path.
  it('one arg ending in .md (but not strict grammar) → filename, not recipient', () => {
    expect(splitReadPositionals(['nope.md'])).toEqual({
      filename: 'nope.md',
    });
    expect(splitReadPositionals(['does-not-exist.md'])).toEqual({
      filename: 'does-not-exist.md',
    });
  });

  it('two args → recipient, filename', () => {
    expect(
      splitReadPositionals(['bob', '1714826789010-aaaaaa.md'])
    ).toEqual({ recipient: 'bob', filename: '1714826789010-aaaaaa.md' });
  });

  it('three args → throws', () => {
    expect(() => splitReadPositionals(['a', 'b', 'c'])).toThrowError(
      /too many arguments/
    );
  });
});

// ─── --json (issue #7) ─────────────────────────────────────────────────────

describe('cmdRead — fm/recipient on result', () => {
  it('formatted mode populates fm with parsed frontmatter', () => {
    setupIdentity('bob');
    writeFile(
      'bob',
      '1714826789010-aaaaaa.md',
      '---\nfrom: alice\nsubject: hi\n---\nthe body\n'
    );
    const r = cmdRead(baseInput());
    expect(r.fm).toEqual({ from: 'alice', subject: 'hi' });
    expect(r.recipient).toBe('bob');
  });

  it('raw mode returns empty fm (no parse cost)', () => {
    setupIdentity('bob');
    writeFile(
      'bob',
      '1714826789010-aaaaaa.md',
      '---\nfrom: alice\n---\nbody\n'
    );
    const r = cmdRead(baseInput({ raw: true }));
    expect(r.fm).toEqual({});
    expect(r.recipient).toBe('bob');
  });

  it('untyped file returns empty fm', () => {
    setupIdentity('bob');
    writeFile('bob', '1714826789010-aaaaaa.md', 'no frontmatter here\n');
    const r = cmdRead(baseInput());
    expect(r.fm).toEqual({});
    expect(r.untyped).toBe(true);
  });
});

describe('buildReadJsonShape', () => {
  function read(content: string): ReturnType<typeof cmdRead> {
    setupIdentity('bob');
    writeFile('bob', '1714826789010-aaaaaa.md', content);
    return cmdRead(baseInput());
  }

  it('mirrors the MCP coord_msg_read shape: { filename, identity, folder, message }', () => {
    const r = read('---\nfrom: alice\nsubject: hi\n---\nthe body\n');
    const j = buildReadJsonShape('1714826789010-aaaaaa.md', r);
    expect(j).toEqual({
      filename: '1714826789010-aaaaaa.md',
      identity: 'bob',
      folder: 'inbox',
      message: { from: 'alice', subject: 'hi', body: 'the body\n' },
    });
  });

  it('archive folder is reflected', () => {
    setupIdentity('bob');
    writeFile(
      'bob',
      '1714826789010-aaaaaa.md',
      '---\nfrom: alice\n---\nb\n',
      'archive'
    );
    const r = cmdRead(baseInput({ fromArchive: true }));
    const j = buildReadJsonShape('1714826789010-aaaaaa.md', r);
    expect(j.folder).toBe('archive');
  });

  it('omits optional keys absent from frontmatter', () => {
    const r = read('---\nfrom: alice\n---\nbody\n');
    const j = buildReadJsonShape('1714826789010-aaaaaa.md', r);
    expect(j.message).toEqual({ from: 'alice', body: 'body\n' });
    expect('subject' in j.message).toBe(false);
    expect('inReplyTo' in j.message).toBe(false);
    expect('tags' in j.message).toBe(false);
    expect('priority' in j.message).toBe(false);
  });

  it('renames in-reply-to → inReplyTo (camelCase like MCP)', () => {
    const r = read(
      '---\nfrom: alice\nin-reply-to: 1714826789000-zzzzzz.md\n---\nbody\n'
    );
    const j = buildReadJsonShape('1714826789010-aaaaaa.md', r);
    expect(j.message.inReplyTo).toBe('1714826789000-zzzzzz.md');
  });

  it('parses inline tags scalar to a string[]', () => {
    const r = read('---\nfrom: alice\ntags: [a, b, c]\n---\nbody\n');
    const j = buildReadJsonShape('1714826789010-aaaaaa.md', r);
    expect(j.message.tags).toEqual(['a', 'b', 'c']);
  });

  it('priority projects only the closed enum (drops bogus values)', () => {
    const r1 = read('---\nfrom: a\npriority: high\n---\nbody\n');
    expect(buildReadJsonShape('1714826789010-aaaaaa.md', r1).message.priority).toBe('high');
    const r2 = read('---\nfrom: a\npriority: bogus\n---\nbody\n');
    expect(buildReadJsonShape('1714826789010-aaaaaa.md', r2).message.priority).toBeUndefined();
  });

  it('untyped message: from is "" and body is the whole file (permissive)', () => {
    const r = read('plain text, no fm\n');
    const j = buildReadJsonShape('1714826789010-aaaaaa.md', r);
    expect(j.message.from).toBe('');
    expect(j.message.body).toBe('plain text, no fm\n');
  });
});

describe('cmdReadCli — --json', () => {
  function run(args: string[]): { stdout: string; stderr: string; code: number } {
    let stdout = '';
    let stderr = '';
    const ctx: CliContext = {
      env: { COORD_ROOT: coordRoot, COORD_IDENTITY: 'bob' },
      coordRoot,
      coordConfig: join(scratch, 'config'),
      stdout: (s) => {
        stdout += s;
      },
      stderr: (s) => {
        stderr += s;
      },
      readStdin: async () => Buffer.alloc(0),
    };
    const code = cmdReadCli(args, ctx);
    return { stdout, stderr, code };
  }

  it('--json prints one line of JSON to stdout, no header on stderr', () => {
    setupIdentity('bob');
    writeFile(
      'bob',
      '1714826789010-aaaaaa.md',
      '---\nfrom: alice\nsubject: hi\n---\nbody\n'
    );
    const r = run(['1714826789010-aaaaaa.md', '--json']);
    expect(r.code).toBe(0);
    expect(r.stderr).toBe('');
    expect(r.stdout.endsWith('\n')).toBe(true);
    const parsed = JSON.parse(r.stdout);
    expect(parsed).toEqual({
      filename: '1714826789010-aaaaaa.md',
      identity: 'bob',
      folder: 'inbox',
      message: { from: 'alice', subject: 'hi', body: 'body\n' },
    });
  });

  it('--json + --raw is rejected', () => {
    setupIdentity('bob');
    writeFile('bob', '1714826789010-aaaaaa.md', '---\nfrom: a\n---\nb\n');
    expect(() =>
      run(['1714826789010-aaaaaa.md', '--json', '--raw'])
    ).toThrowError(/mutually exclusive/);
  });

  it('--json on archive (with --archive) emits folder: "archive"', () => {
    setupIdentity('bob');
    writeFile(
      'bob',
      '1714826789010-aaaaaa.md',
      '---\nfrom: alice\n---\nbody\n',
      'archive'
    );
    const r = run(['1714826789010-aaaaaa.md', '--json', '--archive']);
    expect(r.code).toBe(0);
    expect(JSON.parse(r.stdout).folder).toBe('archive');
  });

  it('--json on an untyped file: from="" and body is whole text', () => {
    setupIdentity('bob');
    writeFile('bob', '1714826789010-aaaaaa.md', 'untyped\n');
    const r = run(['1714826789010-aaaaaa.md', '--json']);
    const parsed = JSON.parse(r.stdout);
    expect(parsed.message).toEqual({ from: '', body: 'untyped\n' });
  });

  it('help string mentions --json', () => {
    const r = run(['--help']);
    expect(r.code).toBe(0);
    expect(r.stderr).toContain('--json');
  });
});
