// tests/unit/context.test.ts — brief-024 context/ v1 core.
//
// Absent-able is the load-bearing property: every verb must handle a
// missing folder / missing files without crashing. That's what lets
// evals-claude's restart-continuity eval A/B a control arm (no
// context/) against a treatment arm that resumes.

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

import type { CliContext } from '../../src/cli-context.ts';
import {
  cmdContextAppend,
  cmdContextCli,
  cmdContextRead,
  cmdContextWrite,
} from '../../src/commands/context.ts';

let scratch: string;
let coordRoot: string;
let stdoutBuf: string;
let stderrBuf: string;
let ctx: CliContext;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-context-test-'));
  coordRoot = join(scratch, 'coord');
  mkdirSync(coordRoot, { recursive: true });
  // Create alice's identity dirs so resolveIdentity is happy. The
  // context/ folder is intentionally absent — that's the point of the
  // absent-able tests.
  mkdirSync(join(coordRoot, 'alice', 'inbox'), { recursive: true });
  mkdirSync(join(coordRoot, 'alice', 'archive'), { recursive: true });
  stdoutBuf = '';
  stderrBuf = '';
  ctx = {
    env: { COORD_IDENTITY: 'alice' },
    coordRoot,
    coordConfig: '/unused',
    stdout: (s) => {
      stdoutBuf += s;
    },
    stderr: (s) => {
      stderrBuf += s;
    },
    readStdin: async () => Buffer.alloc(0),
  };
});

afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

// ─── absent-able: cold agent, no context/ folder ─────────────────────────

describe('cmdContextRead — absent-able', () => {
  it('missing context/ folder → text is empty, absent is true', () => {
    const r = cmdContextRead({
      env: ctx.env,
      coordRoot,
    });
    expect(r.identity).toBe('alice');
    expect(r.file).toBe('now');
    expect(r.text).toBe('');
    expect(r.absent).toBe(true);
    // Sanity: the read must NOT have created the folder — the eval's
    // control arm relies on "no context/" staying that way through a
    // read.
    expect(existsSync(join(coordRoot, 'alice', 'context'))).toBe(false);
  });

  it('--decisions on a missing folder returns empty', () => {
    const r = cmdContextRead({
      file: 'decisions',
      env: ctx.env,
      coordRoot,
    });
    expect(r.text).toBe('');
    expect(r.absent).toBe(true);
  });

  it('--full on a missing folder returns empty', () => {
    const r = cmdContextRead({
      file: 'full',
      env: ctx.env,
      coordRoot,
    });
    expect(r.text).toBe('');
    expect(r.absent).toBe(true);
  });

  it('--full with one surface present is not absent', () => {
    mkdirSync(join(coordRoot, 'alice', 'context'));
    writeFileSync(
      join(coordRoot, 'alice', 'context', 'now.md'),
      'mid-task\n'
    );
    const r = cmdContextRead({
      file: 'full',
      env: ctx.env,
      coordRoot,
    });
    expect(r.absent).toBe(false);
    expect(r.text).toContain('# now.md');
    expect(r.text).toContain('mid-task');
    expect(r.text).not.toContain('# decisions/');
  });

  it('--decisions on an empty decisions/ folder returns absent', () => {
    // Load-bearing for the eval control arm: an empty folder is
    // functionally equivalent to no folder — the log has zero entries
    // in both cases, and the caller must see `absent: true` either
    // way so a rehydrate step can distinguish "no prior context" from
    // "no decisions recorded yet on this task."
    mkdirSync(join(coordRoot, 'alice', 'context', 'decisions'), {
      recursive: true,
    });
    const r = cmdContextRead({
      file: 'decisions',
      env: ctx.env,
      coordRoot,
    });
    expect(r.text).toBe('');
    expect(r.absent).toBe(true);
  });
});

// ─── write: whole-file rewrite ───────────────────────────────────────────

describe('cmdContextWrite', () => {
  it('creates the context/ folder + writes now.md with a trailing newline', () => {
    const r = cmdContextWrite({
      body: 'brief-024 v1 in-flight',
      env: ctx.env,
      coordRoot,
    });
    expect(r.path).toBe(join(coordRoot, 'alice', 'context', 'now.md'));
    // Trailing newline enforced.
    const raw = readFileSync(r.path, 'utf8');
    expect(raw).toBe('brief-024 v1 in-flight\n');
    expect(r.bytes).toBe(raw.length);
  });

  it('preserves an existing trailing newline (no double-newline)', () => {
    const r = cmdContextWrite({
      body: 'already ends in newline\n',
      env: ctx.env,
      coordRoot,
    });
    expect(readFileSync(r.path, 'utf8')).toBe('already ends in newline\n');
  });

  it('overwrites now.md on a subsequent call (whole-file rewrite discipline)', () => {
    cmdContextWrite({
      body: 'first',
      env: ctx.env,
      coordRoot,
    });
    cmdContextWrite({
      body: 'second',
      env: ctx.env,
      coordRoot,
    });
    expect(
      readFileSync(join(coordRoot, 'alice', 'context', 'now.md'), 'utf8')
    ).toBe('second\n');
  });

  it('write does not touch the decisions/ folder', () => {
    cmdContextWrite({
      body: 'now-only',
      env: ctx.env,
      coordRoot,
    });
    expect(
      existsSync(join(coordRoot, 'alice', 'context', 'decisions'))
    ).toBe(false);
  });

  it('does not leak the tmp file when the write succeeds', () => {
    cmdContextWrite({
      body: 'x',
      env: ctx.env,
      coordRoot,
    });
    const entries = require('node:fs').readdirSync(
      join(coordRoot, 'alice', 'context')
    ) as string[];
    // Only now.md; no `.context.tmp-*` sibling should remain.
    expect(entries.filter((n) => n.startsWith('.context.tmp'))).toEqual([]);
  });
});

// ─── append: one file per decision entry ─────────────────────────────────

describe('cmdContextAppend', () => {
  const ts = '2026-07-02T22:00:00.000Z';
  // Same-second t1 vs t2 to prove that filename-sort still equals
  // ISO-time-sort at ms granularity, and that same-ms writes don't
  // collide (rand6 disambiguates).
  const tsA = '2026-07-02T22:00:00.100Z';
  const tsB = '2026-07-02T22:00:00.200Z';

  it('creates the decisions/ folder + writes one entry file with a bulleted body', () => {
    const r = cmdContextAppend({
      decision: 'pick auto as default',
      why: 'preserves pre-brief-023 behavior',
      timestamp: ts,
      env: ctx.env,
      coordRoot,
    });
    // Filename shape: <unix-ms>-<rand6>.md (LAYOUT-004 grammar).
    expect(r.filename).toMatch(/^\d+-[0-9a-z]{6}\.md$/);
    expect(r.path).toBe(
      join(coordRoot, 'alice', 'context', 'decisions', r.filename)
    );
    // Filename's ms prefix must match the body's ISO timestamp so
    // filename-sort equals chronological-sort at ms granularity.
    const [msStr] = r.filename.split('-');
    expect(Number(msStr)).toBe(Date.parse(ts));
    expect(r.line).toBe(
      `- ${ts} pick auto as default. why: preserves pre-brief-023 behavior.`
    );
    expect(readFileSync(r.path, 'utf8')).toBe(r.line + '\n');
  });

  it('two appends land in TWO distinct files, both surviving (no rewrite, no clobber)', () => {
    const rA = cmdContextAppend({
      decision: 'first',
      why: 'a',
      timestamp: tsA,
      env: ctx.env,
      coordRoot,
    });
    const rB = cmdContextAppend({
      decision: 'second',
      why: 'b',
      timestamp: tsB,
      env: ctx.env,
      coordRoot,
    });
    expect(rA.filename).not.toBe(rB.filename);
    const dir = join(coordRoot, 'alice', 'context', 'decisions');
    const entries = require('node:fs').readdirSync(dir) as string[];
    expect(entries.length).toBe(2);
    // Filename-sort order == chronological order (tsA < tsB).
    const sorted = [...entries].sort();
    expect(sorted[0]).toBe(rA.filename);
    expect(sorted[1]).toBe(rB.filename);
    // Neither file was rewritten by the other's append — bodies are
    // exactly what each call returned.
    expect(readFileSync(join(dir, rA.filename), 'utf8')).toBe(rA.line + '\n');
    expect(readFileSync(join(dir, rB.filename), 'utf8')).toBe(rB.line + '\n');
  });

  it('two appends with the SAME timestamp collide on ms-prefix but rand6 disambiguates', () => {
    // Same body-ts → same ms prefix on both files. The random suffix
    // is what stops them from writing to the same path. This is the
    // whole reason for the folder-of-files shape: no read-modify-
    // write, no race, one-shot atomic create.
    const r1 = cmdContextAppend({
      decision: 'a',
      why: 'why-a',
      timestamp: ts,
      env: ctx.env,
      coordRoot,
    });
    const r2 = cmdContextAppend({
      decision: 'b',
      why: 'why-b',
      timestamp: ts,
      env: ctx.env,
      coordRoot,
    });
    const [ms1] = r1.filename.split('-');
    const [ms2] = r2.filename.split('-');
    expect(ms1).toBe(ms2); // same ms
    expect(r1.filename).not.toBe(r2.filename); // different rand6
    // Both files present, both readable.
    expect(readFileSync(r1.path, 'utf8')).toBe(r1.line + '\n');
    expect(readFileSync(r2.path, 'utf8')).toBe(r2.line + '\n');
  });

  it('accepts an optional filename test seam for deterministic assertions', () => {
    // Real callers (CLI, MCP, SDK handle) never set this — but the
    // seam lets tests assert exact-bytes-on-disk without depending
    // on rand6() output.
    const r = cmdContextAppend({
      decision: 'seeded',
      why: 'seed',
      timestamp: ts,
      filename: '1234567890-aaaaaa.md',
      env: ctx.env,
      coordRoot,
    });
    expect(r.filename).toBe('1234567890-aaaaaa.md');
    expect(existsSync(r.path)).toBe(true);
  });

  it('strips duplicate trailing period from decision + why', () => {
    const r = cmdContextAppend({
      decision: 'has a period.',
      why: 'also has a period.',
      timestamp: ts,
      env: ctx.env,
      coordRoot,
    });
    expect(r.line).toBe(
      `- ${ts} has a period. why: also has a period.`
    );
  });

  it('rejects empty decision or why', () => {
    expect(() =>
      cmdContextAppend({
        decision: '',
        why: 'reason',
        timestamp: ts,
        env: ctx.env,
        coordRoot,
      })
    ).toThrow(/--decision is required/);
    expect(() =>
      cmdContextAppend({
        decision: 'thing',
        why: '   ',
        timestamp: ts,
        env: ctx.env,
        coordRoot,
      })
    ).toThrow(/--why is required/);
  });

  it('rejects multi-line decision or why', () => {
    expect(() =>
      cmdContextAppend({
        decision: 'line one\nline two',
        why: 'reason',
        timestamp: ts,
        env: ctx.env,
        coordRoot,
      })
    ).toThrow(/single lines/);
  });

  it('append does not touch now.md', () => {
    cmdContextAppend({
      decision: 'thing',
      why: 'reason',
      timestamp: ts,
      env: ctx.env,
      coordRoot,
    });
    expect(
      existsSync(join(coordRoot, 'alice', 'context', 'now.md'))
    ).toBe(false);
  });

  it('decisions/ entries survive after a now.md write (independent surfaces)', () => {
    const r = cmdContextAppend({
      decision: 'thing',
      why: 'reason',
      timestamp: ts,
      env: ctx.env,
      coordRoot,
    });
    cmdContextWrite({
      body: 'now',
      env: ctx.env,
      coordRoot,
    });
    expect(readFileSync(r.path, 'utf8')).toContain('thing. why: reason.');
  });

  it('unparseable timestamp falls back to msNow() for the filename prefix', () => {
    const r = cmdContextAppend({
      decision: 'x',
      why: 'y',
      timestamp: 'not-a-real-iso-string',
      env: ctx.env,
      coordRoot,
    });
    // Filename still parses as LAYOUT-004; ms prefix is a real number.
    expect(r.filename).toMatch(/^\d+-[0-9a-z]{6}\.md$/);
    const [msStr] = r.filename.split('-');
    // Not NaN, not the parsed-then-fallback value — an actual current-
    // clock ms. Loose sanity range: >= 2024-01-01 in ms.
    expect(Number(msStr)).toBeGreaterThan(1704067200000);
  });
});

// ─── read after write / append ───────────────────────────────────────────

describe('cmdContextRead — after write / append', () => {
  it('reads back exactly what write wrote', () => {
    cmdContextWrite({
      body: '# now\nstate\n',
      env: ctx.env,
      coordRoot,
    });
    const r = cmdContextRead({
      env: ctx.env,
      coordRoot,
    });
    expect(r.text).toBe('# now\nstate\n');
    expect(r.absent).toBe(false);
  });

  it('reads decisions with --decisions', () => {
    cmdContextAppend({
      decision: 'x',
      why: 'y',
      timestamp: '2026-07-02T00:00:00.000Z',
      env: ctx.env,
      coordRoot,
    });
    const r = cmdContextRead({
      file: 'decisions',
      env: ctx.env,
      coordRoot,
    });
    expect(r.text).toContain('x. why: y.');
    expect(r.absent).toBe(false);
  });

  it('reads both with --full', () => {
    cmdContextWrite({
      body: 'now-state',
      env: ctx.env,
      coordRoot,
    });
    cmdContextAppend({
      decision: 'x',
      why: 'y',
      timestamp: '2026-07-02T00:00:00.000Z',
      env: ctx.env,
      coordRoot,
    });
    const r = cmdContextRead({
      file: 'full',
      env: ctx.env,
      coordRoot,
    });
    expect(r.text).toContain('# now.md');
    expect(r.text).toContain('now-state');
    expect(r.text).toContain('# decisions/');
    expect(r.text).toContain('x. why: y.');
    expect(r.absent).toBe(false);
  });

  it('read --decisions concatenates entries in filename-sort order', () => {
    // Prove the chronological-order guarantee: earlier timestamps
    // appear earlier in the concatenated output, regardless of
    // insertion order.
    cmdContextAppend({
      decision: 'later',
      why: 'y',
      timestamp: '2026-07-02T02:00:00.000Z',
      env: ctx.env,
      coordRoot,
    });
    cmdContextAppend({
      decision: 'earlier',
      why: 'y',
      timestamp: '2026-07-02T01:00:00.000Z',
      env: ctx.env,
      coordRoot,
    });
    const r = cmdContextRead({
      file: 'decisions',
      env: ctx.env,
      coordRoot,
    });
    const earlierIdx = r.text.indexOf('earlier');
    const laterIdx = r.text.indexOf('later');
    expect(earlierIdx).toBeGreaterThanOrEqual(0);
    expect(laterIdx).toBeGreaterThan(earlierIdx);
  });

  it('read --decisions ignores non-.md siblings in the folder', () => {
    // Someone drops a .DS_Store or a swap file into decisions/ — we
    // must not treat it as an entry. Same defensive stance as the
    // message ls layer.
    cmdContextAppend({
      decision: 'real entry',
      why: 'y',
      timestamp: '2026-07-02T00:00:00.000Z',
      env: ctx.env,
      coordRoot,
    });
    writeFileSync(
      join(coordRoot, 'alice', 'context', 'decisions', '.DS_Store'),
      'binary junk'
    );
    writeFileSync(
      join(coordRoot, 'alice', 'context', 'decisions', 'notes.txt'),
      'stray text file'
    );
    const r = cmdContextRead({
      file: 'decisions',
      env: ctx.env,
      coordRoot,
    });
    expect(r.text).toContain('real entry');
    expect(r.text).not.toContain('binary junk');
    expect(r.text).not.toContain('stray text file');
  });
});

// ─── CLI wrapper ─────────────────────────────────────────────────────────

describe('cmdContextCli', () => {
  it('read on empty prints nothing and exits 0 (absent-able)', async () => {
    // Load-bearing for the SessionStart hook: `coord context read` on a
    // fresh agent must be exit-0 + no output so hooks can `cat` it
    // unconditionally.
    const rc = await cmdContextCli(['read'], ctx);
    expect(rc).toBe(0);
    expect(stdoutBuf).toBe('');
  });

  it('write reads from ctx.readStdin and reports bytes', async () => {
    ctx.readStdin = async () => Buffer.from('body from stdin');
    const rc = await cmdContextCli(['write'], ctx);
    expect(rc).toBe(0);
    expect(stdoutBuf).toMatch(/wrote 16 bytes to .*context\/now\.md/);
    // The read-round-trip proves write actually persisted.
    stdoutBuf = '';
    await cmdContextCli(['read'], ctx);
    expect(stdoutBuf).toBe('body from stdin\n');
  });

  it('append requires --decision and --why', async () => {
    await expect(
      cmdContextCli(['append', '--decision', 'x'], ctx)
    ).rejects.toThrow(/--why/);
    await expect(
      cmdContextCli(['append', '--why', 'y'], ctx)
    ).rejects.toThrow(/--decision/);
  });

  it('append prints the exact line written', async () => {
    const rc = await cmdContextCli(
      ['append', '--decision', 'thing', '--why', 'reason'],
      ctx
    );
    expect(rc).toBe(0);
    expect(stdoutBuf).toMatch(
      /^- \d{4}-\d{2}-\d{2}T[\d:.]+Z thing\. why: reason\.\n$/
    );
  });

  it('unknown verb → exit 2 + help on stderr', async () => {
    const rc = await cmdContextCli(['banana'], ctx);
    expect(rc).toBe(2);
    expect(stderrBuf).toMatch(/unknown subcommand/);
    expect(stderrBuf).toMatch(/usage: coord context/);
  });

  it('no verb → exit 2 + help', async () => {
    const rc = await cmdContextCli([], ctx);
    expect(rc).toBe(2);
    expect(stderrBuf).toMatch(/usage: coord context/);
  });

  it('--help → exit 0 + help on stderr', async () => {
    const rc = await cmdContextCli(['--help'], ctx);
    expect(rc).toBe(0);
    expect(stderrBuf).toMatch(/usage: coord context/);
  });

  it('read with positional identity reads that peer\'s context', async () => {
    // Set up a peer with a context/now.md.
    mkdirSync(join(coordRoot, 'bob', 'inbox'), { recursive: true });
    mkdirSync(join(coordRoot, 'bob', 'archive'), { recursive: true });
    mkdirSync(join(coordRoot, 'bob', 'context'), { recursive: true });
    writeFileSync(
      join(coordRoot, 'bob', 'context', 'now.md'),
      "bob's state\n"
    );
    const rc = await cmdContextCli(['read', 'bob'], ctx);
    expect(rc).toBe(0);
    expect(stdoutBuf).toBe("bob's state\n");
  });
});
