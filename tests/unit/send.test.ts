// tests/unit/send.test.ts — comprehensive coverage of cmd_send.

import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type { CliContext } from '../../src/cli-context.ts';
import { parseFrontmatter, validFilename } from '../../src/common.ts';
import { cmdSend, cmdSendCli, type SendInput } from '../../src/commands/send.ts';

let scratch: string;
let stRoot: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-send-test-'));
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

function baseInput(overrides: Partial<SendInput> = {}): SendInput {
  return {
    to: 'bob',
    from: 'alice',
    body: 'hello bob',
    env: {} as NodeJS.ProcessEnv,
    stRoot,
    ...overrides,
  };
}

describe('cmdSend — happy path', () => {
  it('writes a well-formed file directly into <to>/inbox/', () => {
    setupIdentity('alice');
    const r = cmdSend(baseInput({ subject: 'hi' }));
    expect(validFilename(r.filename)).toBe(true);
    expect(r.path).toBe(join(stRoot, 'bob', 'inbox', r.filename));
    expect(existsSync(r.path)).toBe(true);
    const text = readFileSync(r.path, 'utf8');
    const parsed = parseFrontmatter(text);
    expect(parsed.fm.from).toBe('alice');
    expect(parsed.fm.subject).toBe('hi');
    expect(parsed.body).toBe('hello bob\n');
  });

  it('creates <to>/inbox/ on the fly when the recipient is not yet hosted', () => {
    setupIdentity('alice');
    expect(existsSync(join(stRoot, 'newcomer'))).toBe(false);
    const r = cmdSend(baseInput({ to: 'newcomer' }));
    expect(existsSync(r.path)).toBe(true);
  });

  it('does NOT mutate sender state (no spurious files in <from>/inbox/)', () => {
    setupIdentity('alice');
    cmdSend(baseInput());
    const ls = require('node:fs').readdirSync(join(stRoot, 'alice', 'inbox'));
    expect(ls).toEqual([]);
  });

  it('the LAYOUT-004 frontmatter has only `from:` (no to:/ts:)', () => {
    setupIdentity('alice');
    const r = cmdSend(baseInput());
    const text = readFileSync(r.path, 'utf8');
    expect(text).toMatch(/^---\nfrom: alice\n---\n/);
    expect(text).not.toMatch(/^to:/m);
    expect(text).not.toMatch(/^ts:/m);
  });
});

describe('cmdSend — recipient validation', () => {
  it('rejects an empty <to>', () => {
    setupIdentity('alice');
    expect(() => cmdSend(baseInput({ to: '' }))).toThrowError(/<to> is required/);
  });

  it('rejects an invalid recipient name (uppercase)', () => {
    setupIdentity('alice');
    expect(() => cmdSend(baseInput({ to: 'BOB' }))).toThrowError(
      /invalid (agent name|identity)/
    );
  });

  it('rejects a reserved recipient name (inbox)', () => {
    setupIdentity('alice');
    expect(() => cmdSend(baseInput({ to: 'inbox' }))).toThrowError(
      /invalid (agent name|identity)/
    );
  });

  it('rejects a reserved recipient name (status)', () => {
    setupIdentity('alice');
    expect(() => cmdSend(baseInput({ to: 'status' }))).toThrowError(
      /invalid (agent name|identity)/
    );
  });

  it('rejects leading hyphen', () => {
    setupIdentity('alice');
    expect(() => cmdSend(baseInput({ to: '-alice' }))).toThrowError(
      /invalid (agent name|identity)/
    );
  });
});

describe('cmdSend — --from resolution', () => {
  it('uses explicit --from when provided', () => {
    setupIdentity('alice');
    setupIdentity('charlie');
    const r = cmdSend(
      baseInput({
        from: 'charlie',
        env: { ST_AGENT: 'alice' } as NodeJS.ProcessEnv,
      })
    );
    expect(parseFrontmatter(readFileSync(r.path, 'utf8')).fm.from).toBe(
      'charlie'
    );
  });

  it('falls back to ST_AGENT when --from omitted', () => {
    setupIdentity('alice');
    const r = cmdSend(
      baseInput({
        from: undefined,
        env: { ST_AGENT: 'alice' } as NodeJS.ProcessEnv,
      })
    );
    expect(parseFrontmatter(readFileSync(r.path, 'utf8')).fm.from).toBe('alice');
  });

  it('errors mentioning ST_AGENT when neither --from nor env is set', () => {
    expect(() =>
      cmdSend(baseInput({ from: undefined, env: {} as NodeJS.ProcessEnv }))
    ).toThrowError(/ST_AGENT/);
  });

  it('errors with mkdir hint when --from points at a non-hosted identity', () => {
    setupIdentity('alice');
    expect(() =>
      cmdSend(baseInput({ from: 'ghost' }))
    ).toThrowError(/(agent|identity) folder missing for ghost/);
  });

  it('errors when --from is an invalid identity name', () => {
    expect(() =>
      cmdSend(baseInput({ from: 'INVALID' }))
    ).toThrowError(/invalid (agent name|identity)/);
  });
});

describe('cmdSend — body handling', () => {
  it('rejects empty body', () => {
    setupIdentity('alice');
    expect(() => cmdSend(baseInput({ body: '' }))).toThrowError(
      /message body is empty/
    );
  });

  it('rejects empty Buffer body', () => {
    setupIdentity('alice');
    expect(() => cmdSend(baseInput({ body: Buffer.alloc(0) }))).toThrowError(
      /message body is empty/
    );
  });

  it('preserves a 1MB body byte-for-byte', () => {
    setupIdentity('alice');
    const huge = Buffer.alloc(1024 * 1024, 'a');
    const r = cmdSend(baseInput({ body: huge }));
    const written = readFileSync(r.path);
    // file = head + body + (trailing \n if not already)
    expect(written.length).toBeGreaterThanOrEqual(huge.length);
    expect(written.slice(written.length - huge.length - 1, -1)).toEqual(huge);
  });

  it('does NOT add a duplicate newline if body already ends in one', () => {
    setupIdentity('alice');
    const r = cmdSend(baseInput({ body: 'line1\nline2\n' }));
    expect(readFileSync(r.path, 'utf8')).toBe(
      '---\nfrom: alice\n---\nline1\nline2\n'
    );
  });

  it('adds a trailing newline if body lacks one', () => {
    setupIdentity('alice');
    const r = cmdSend(baseInput({ body: 'no-newline' }));
    expect(readFileSync(r.path, 'utf8').endsWith('no-newline\n')).toBe(true);
  });

  it('binary body (Buffer with non-utf8 bytes) survives', () => {
    setupIdentity('alice');
    const bytes = Buffer.from([0x00, 0x01, 0x80, 0xff]);
    const r = cmdSend(baseInput({ body: bytes }));
    const written = readFileSync(r.path);
    expect(written.subarray(written.length - 5, written.length - 1)).toEqual(
      bytes
    );
  });
});

describe('cmdSend — --subject', () => {
  it('quotes a subject with embedded colons', () => {
    setupIdentity('alice');
    const r = cmdSend(baseInput({ subject: 're: hello' }));
    expect(readFileSync(r.path, 'utf8')).toContain('subject: "re: hello"');
  });

  it('roundtrip: embedded "quote"s and \\backslashes survive', () => {
    setupIdentity('alice');
    const subject = 'has "quotes" and \\backslash';
    const r = cmdSend(baseInput({ subject }));
    const parsed = parseFrontmatter(readFileSync(r.path, 'utf8'));
    expect(parsed.fm.subject).toBe(subject);
  });

  it('roundtrip: embedded newline survives via yaml \\n escape', () => {
    setupIdentity('alice');
    const subject = 'first\nsecond';
    const r = cmdSend(baseInput({ subject }));
    const parsed = parseFrontmatter(readFileSync(r.path, 'utf8'));
    expect(parsed.fm.subject).toBe(subject);
  });

  it('omits subject when undefined', () => {
    setupIdentity('alice');
    const r = cmdSend(baseInput({ subject: undefined }));
    expect(readFileSync(r.path, 'utf8')).not.toContain('subject:');
  });

  it('omits subject when empty string', () => {
    setupIdentity('alice');
    const r = cmdSend(baseInput({ subject: '' }));
    expect(readFileSync(r.path, 'utf8')).not.toContain('subject:');
  });
});

describe('cmdSend — --in-reply-to', () => {
  it('accepts a valid filename', () => {
    setupIdentity('alice');
    const r = cmdSend(
      baseInput({ inReplyTo: '1714826789012-abcdef.md' })
    );
    expect(readFileSync(r.path, 'utf8')).toContain(
      'in-reply-to: 1714826789012-abcdef.md'
    );
  });

  it('rejects an invalid filename grammar', () => {
    setupIdentity('alice');
    expect(() => cmdSend(baseInput({ inReplyTo: 'garbage' }))).toThrowError(
      /invalid filename/
    );
  });

  it('rejects legacy 3-segment filename', () => {
    setupIdentity('alice');
    expect(() =>
      cmdSend(baseInput({ inReplyTo: '1714826789012-operator-abcdef.md' }))
    ).toThrowError(/invalid filename/);
  });
});

describe('cmdSend — --tags', () => {
  // brief-017a bug 4: plain-scalar tags emit unquoted in the flow list.
  it('accepts comma-separated string and emits inline list', () => {
    setupIdentity('alice');
    const r = cmdSend(baseInput({ tags: 'review, planning' }));
    expect(readFileSync(r.path, 'utf8')).toContain(
      'tags: [review, planning]'
    );
  });

  it('trims whitespace around each tag', () => {
    setupIdentity('alice');
    const r = cmdSend(baseInput({ tags: '  a , b  , c' }));
    expect(readFileSync(r.path, 'utf8')).toContain('tags: [a, b, c]');
  });

  it('drops empty tag entries', () => {
    setupIdentity('alice');
    const r = cmdSend(baseInput({ tags: 'a,,b' }));
    expect(readFileSync(r.path, 'utf8')).toContain('tags: [a, b]');
  });

  it('omits tags when only commas/whitespace', () => {
    setupIdentity('alice');
    const r = cmdSend(baseInput({ tags: '  ,  ,' }));
    expect(readFileSync(r.path, 'utf8')).not.toContain('tags:');
  });

  it('accepts an array form too', () => {
    setupIdentity('alice');
    const r = cmdSend(baseInput({ tags: ['x', 'y'] }));
    expect(readFileSync(r.path, 'utf8')).toContain('tags: [x, y]');
  });
});

describe('cmdSend — --priority', () => {
  it.each(['low', 'normal', 'high'])('accepts %s', (p) => {
    setupIdentity('alice');
    const r = cmdSend(baseInput({ priority: p }));
    expect(readFileSync(r.path, 'utf8')).toContain(`priority: ${p}`);
  });

  it('rejects an unknown priority', () => {
    setupIdentity('alice');
    expect(() => cmdSend(baseInput({ priority: 'urgent' }))).toThrowError(
      /priority must be one of/
    );
  });

  it('rejects empty-string priority? — empty is treated as omitted', () => {
    setupIdentity('alice');
    const r = cmdSend(baseInput({ priority: '' }));
    expect(readFileSync(r.path, 'utf8')).not.toContain('priority:');
  });
});

describe('cmdSend — uniqueness under rapid fire', () => {
  it('30 rapid calls produce 30 distinct filenames in the inbox', () => {
    setupIdentity('alice');
    const seen = new Set<string>();
    for (let i = 0; i < 30; i++) {
      const r = cmdSend(baseInput({ body: `msg-${i}` }));
      expect(seen.has(r.filename)).toBe(false);
      seen.add(r.filename);
    }
    const ls = require('node:fs').readdirSync(join(stRoot, 'bob', 'inbox'));
    expect(ls).toHaveLength(30);
  });
});

// ─── cmdSendCli — `-m`/`--message` inline body alias (brief-033) ────────

describe('cmdSendCli — -m flag (brief-033)', () => {
  function makeCtx(opts: {
    stdin?: string;
    stdinIsTty?: boolean;
  } = {}): CliContext & { stdoutBuf: string[] } {
    const stdoutBuf: string[] = [];
    const ctx = {
      env: { ST_AGENT: 'alice' } as NodeJS.ProcessEnv,
      stRoot,
      stConfig: join(scratch, 'config'),
      stdout: (s: string) => stdoutBuf.push(s),
      stderr: () => {},
      readStdin: async () => Buffer.from(opts.stdin ?? '', 'utf8'),
      stdinIsTty: () => opts.stdinIsTty ?? true,
    } as CliContext;
    return Object.assign(ctx, { stdoutBuf });
  }

  it('-m "<body>" writes a message with that body, no stdin read', async () => {
    setupIdentity('alice');
    setupIdentity('bob');
    const ctx = makeCtx({ stdinIsTty: true });
    const rc = await cmdSendCli(['bob', '-m', 'hi inline'], ctx);
    expect(rc).toBe(0);
    const filename = ctx.stdoutBuf.join('').trim();
    expect(validFilename(filename)).toBe(true);
    const content = readFileSync(
      join(stRoot, 'bob', 'inbox', filename),
      'utf8'
    );
    expect(parseFrontmatter(content).body).toBe('hi inline\n');
  });

  it('--message <body> long-form alias works the same', async () => {
    setupIdentity('alice');
    setupIdentity('bob');
    const ctx = makeCtx({ stdinIsTty: true });
    const rc = await cmdSendCli(
      ['bob', '--message', 'hi via long form'],
      ctx
    );
    expect(rc).toBe(0);
    const filename = ctx.stdoutBuf.join('').trim();
    const content = readFileSync(
      join(stRoot, 'bob', 'inbox', filename),
      'utf8'
    );
    expect(parseFrontmatter(content).body).toBe('hi via long form\n');
  });

  it('no -m → reads stdin (regression of current behavior)', async () => {
    setupIdentity('alice');
    setupIdentity('bob');
    const ctx = makeCtx({ stdin: 'from stdin', stdinIsTty: false });
    const rc = await cmdSendCli(['bob'], ctx);
    expect(rc).toBe(0);
    const filename = ctx.stdoutBuf.join('').trim();
    const content = readFileSync(
      join(stRoot, 'bob', 'inbox', filename),
      'utf8'
    );
    expect(parseFrontmatter(content).body).toBe('from stdin\n');
  });

  it('-m AND piped stdin → loud error, no message written', async () => {
    setupIdentity('alice');
    setupIdentity('bob');
    const ctx = makeCtx({
      stdin: 'should-not-be-used',
      stdinIsTty: false,
    });
    await expect(
      cmdSendCli(['bob', '-m', 'inline body'], ctx)
    ).rejects.toThrow(/-m OR stdin, not both/);
    const ls = require('node:fs').readdirSync(
      join(stRoot, 'bob', 'inbox')
    );
    expect(ls).toHaveLength(0);
  });

  it('-m "" → empty body error (matches the no-body-from-stdin path)', async () => {
    setupIdentity('alice');
    setupIdentity('bob');
    const ctx = makeCtx({ stdinIsTty: true });
    await expect(cmdSendCli(['bob', '-m', ''], ctx)).rejects.toThrow(/empty/i);
  });

  it('-m with no value throws "requires a value"', async () => {
    const ctx = makeCtx({ stdinIsTty: true });
    await expect(cmdSendCli(['bob', '-m'], ctx)).rejects.toThrow(
      /-m requires a value/
    );
  });
});
