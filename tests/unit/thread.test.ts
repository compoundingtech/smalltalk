// tests/unit/thread.test.ts — coverage for cmd_thread.

import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  cmdThread,
  formatThreadLine,
  splitThreadPositionals,
  type ThreadInput,
} from '../../src/commands/thread.ts';

let scratch: string;
let stRoot: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-thread-test-'));
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

interface Msg {
  to: string;
  filename: string;
  from: string;
  subject: string;
  inReplyTo?: string;
  folder?: 'inbox' | 'archive';
}

function writeMsg(m: Msg): void {
  const folder = m.folder ?? 'inbox';
  const dir = join(stRoot, m.to, folder);
  mkdirSync(dir, { recursive: true });
  let head = `---\nfrom: ${m.from}\nsubject: ${m.subject}\n`;
  if (m.inReplyTo) head += `in-reply-to: ${m.inReplyTo}\n`;
  head += '---\n';
  writeFileSync(join(dir, m.filename), `${head}body\n`);
}

function input(overrides: Partial<ThreadInput> = {}): ThreadInput {
  return {
    recipient: 'bob',
    filename: '1714826789010-aaaaaa.md',
    env: {} as NodeJS.ProcessEnv,
    stRoot,
    ...overrides,
  };
}

// ─── Singleton ──────────────────────────────────────────────────────────

describe('cmdThread — singleton', () => {
  it('one message, no in-reply-to → one line at depth 0', () => {
    setupIdentity('bob');
    writeMsg({
      to: 'bob',
      filename: '1714826789010-aaaaaa.md',
      from: 'alice',
      subject: 'solo',
    });
    const r = cmdThread(input());
    expect(r.lines).toEqual([
      {
        filename: '1714826789010-aaaaaa.md',
        from: 'alice',
        subject: 'solo',
        depth: 0,
      },
    ]);
  });
});

// ─── Linear chain ──────────────────────────────────────────────────────

describe('cmdThread — linear chain (flat default)', () => {
  it('walks ancestors and prints flat chronological', () => {
    setupIdentity('bob');
    writeMsg({
      to: 'bob',
      filename: '1714826789010-aaaaaa.md',
      from: 'alice',
      subject: 'root',
    });
    writeMsg({
      to: 'bob',
      filename: '1714826789020-bbbbbb.md',
      from: 'alice',
      subject: 'child',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    writeMsg({
      to: 'bob',
      filename: '1714826789030-cccccc.md',
      from: 'alice',
      subject: 'grandchild',
      inReplyTo: '1714826789020-bbbbbb.md',
    });
    const r = cmdThread(
      input({ filename: '1714826789030-cccccc.md' })
    );
    expect(r.lines.map((l) => l.filename)).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789020-bbbbbb.md',
      '1714826789030-cccccc.md',
    ]);
    for (const l of r.lines) expect(l.depth).toBe(0);
  });
});

describe('cmdThread — linear chain (--tree)', () => {
  it('depth-indents the same chain', () => {
    setupIdentity('bob');
    writeMsg({
      to: 'bob',
      filename: '1714826789010-aaaaaa.md',
      from: 'alice',
      subject: 'root',
    });
    writeMsg({
      to: 'bob',
      filename: '1714826789020-bbbbbb.md',
      from: 'alice',
      subject: 'child',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    writeMsg({
      to: 'bob',
      filename: '1714826789030-cccccc.md',
      from: 'alice',
      subject: 'grandchild',
      inReplyTo: '1714826789020-bbbbbb.md',
    });
    const r = cmdThread(
      input({ filename: '1714826789030-cccccc.md', tree: true })
    );
    expect(r.lines.map((l) => l.depth)).toEqual([0, 1, 2]);
  });
});

// ─── Branching descendants ──────────────────────────────────────────────

describe('cmdThread — branching descendants', () => {
  it('flat: all descendants appear, sorted by filename', () => {
    setupIdentity('bob');
    writeMsg({
      to: 'bob',
      filename: '1714826789010-aaaaaa.md',
      from: 'alice',
      subject: 'root',
    });
    writeMsg({
      to: 'bob',
      filename: '1714826789020-bbbbbb.md',
      from: 'alice',
      subject: 'reply1',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    writeMsg({
      to: 'bob',
      filename: '1714826789030-cccccc.md',
      from: 'carol',
      subject: 'reply2',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    writeMsg({
      to: 'bob',
      filename: '1714826789040-dddddd.md',
      from: 'alice',
      subject: 'subreply',
      inReplyTo: '1714826789020-bbbbbb.md',
    });
    const r = cmdThread(input({ filename: '1714826789010-aaaaaa.md' }));
    expect(r.lines.map((l) => l.filename)).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789020-bbbbbb.md',
      '1714826789030-cccccc.md',
      '1714826789040-dddddd.md',
    ]);
  });

  it('--tree shows branch hierarchy with proper depth', () => {
    setupIdentity('bob');
    writeMsg({
      to: 'bob',
      filename: '1714826789010-aaaaaa.md',
      from: 'alice',
      subject: 'root',
    });
    writeMsg({
      to: 'bob',
      filename: '1714826789020-bbbbbb.md',
      from: 'alice',
      subject: 'reply1',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    writeMsg({
      to: 'bob',
      filename: '1714826789030-cccccc.md',
      from: 'carol',
      subject: 'reply2',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    writeMsg({
      to: 'bob',
      filename: '1714826789040-dddddd.md',
      from: 'alice',
      subject: 'subreply',
      inReplyTo: '1714826789020-bbbbbb.md',
    });
    const r = cmdThread(
      input({ filename: '1714826789010-aaaaaa.md', tree: true })
    );
    expect(r.lines.map((l) => ({ f: l.filename, d: l.depth }))).toEqual([
      { f: '1714826789010-aaaaaa.md', d: 0 },
      { f: '1714826789020-bbbbbb.md', d: 1 },
      { f: '1714826789040-dddddd.md', d: 2 },
      { f: '1714826789030-cccccc.md', d: 1 },
    ]);
  });
});

// ─── Cross-identity walk ────────────────────────────────────────────────

describe('cmdThread — cross-identity walk', () => {
  it('reaches messages in other identities via in-reply-to chain', () => {
    setupIdentity('alice');
    setupIdentity('bob');
    // alice → bob (under bob/inbox/)
    writeMsg({
      to: 'bob',
      filename: '1714826789010-aaaaaa.md',
      from: 'alice',
      subject: 'hi bob',
    });
    // bob → alice (under alice/inbox/)
    writeMsg({
      to: 'alice',
      filename: '1714826789020-bbbbbb.md',
      from: 'bob',
      subject: 're hi',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    // alice → bob, follow-up
    writeMsg({
      to: 'bob',
      filename: '1714826789030-cccccc.md',
      from: 'alice',
      subject: 're re hi',
      inReplyTo: '1714826789020-bbbbbb.md',
    });
    // Seed from the middle reply, which lives in alice's tree.
    const r = cmdThread(
      input({ recipient: 'alice', filename: '1714826789020-bbbbbb.md' })
    );
    expect(r.lines.map((l) => l.filename)).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789020-bbbbbb.md',
      '1714826789030-cccccc.md',
    ]);
  });
});

// ─── Inbox + archive ────────────────────────────────────────────────────

describe('cmdThread — spans inbox and archive', () => {
  it('finds an ancestor that has been archived', () => {
    setupIdentity('bob');
    writeMsg({
      to: 'bob',
      filename: '1714826789010-aaaaaa.md',
      from: 'alice',
      subject: 'root',
      folder: 'archive',
    });
    writeMsg({
      to: 'bob',
      filename: '1714826789020-bbbbbb.md',
      from: 'alice',
      subject: 'child',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    const r = cmdThread(input({ filename: '1714826789020-bbbbbb.md' }));
    expect(r.lines.map((l) => l.filename)).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789020-bbbbbb.md',
    ]);
  });
});

// ─── Broken / cyclic data ───────────────────────────────────────────────

describe('cmdThread — robustness', () => {
  it('orphan ancestor (in-reply-to points at missing file): only the seed walks', () => {
    setupIdentity('bob');
    writeMsg({
      to: 'bob',
      filename: '1714826789020-bbbbbb.md',
      from: 'alice',
      subject: 'child',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    const r = cmdThread(input({ filename: '1714826789020-bbbbbb.md' }));
    expect(r.lines.map((l) => l.filename)).toEqual([
      '1714826789020-bbbbbb.md',
    ]);
  });

  it('cycle in in-reply-to (X → Y → X) terminates and emits unique lines', () => {
    setupIdentity('bob');
    writeMsg({
      to: 'bob',
      filename: '1714826789010-aaaaaa.md',
      from: 'alice',
      subject: 'X',
      inReplyTo: '1714826789020-bbbbbb.md',
    });
    writeMsg({
      to: 'bob',
      filename: '1714826789020-bbbbbb.md',
      from: 'alice',
      subject: 'Y',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    const r = cmdThread(input({ filename: '1714826789010-aaaaaa.md' }));
    // Two unique lines, no infinite loop.
    expect(r.lines.map((l) => l.filename).sort()).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789020-bbbbbb.md',
    ]);
  });

  it('empty in-reply-to value treated as no parent', () => {
    setupIdentity('bob');
    // The bash impl emits in-reply-to only when non-empty; simulate by
    // writing the literal "in-reply-to: " (empty value) — the parser
    // returns an empty string which our walk treats as no parent.
    writeFileSync(
      join(stRoot, 'bob', 'inbox', '1714826789010-aaaaaa.md'),
      '---\nfrom: alice\nsubject: solo\nin-reply-to: \n---\nbody\n'
    );
    const r = cmdThread(input());
    expect(r.lines.map((l) => l.filename)).toEqual([
      '1714826789010-aaaaaa.md',
    ]);
  });

  it('parent ref that is not a valid filename is treated as no parent', () => {
    setupIdentity('bob');
    writeFileSync(
      join(stRoot, 'bob', 'inbox', '1714826789010-aaaaaa.md'),
      '---\nfrom: alice\nin-reply-to: garbage\n---\nbody\n'
    );
    const r = cmdThread(input());
    expect(r.lines.map((l) => l.filename)).toEqual([
      '1714826789010-aaaaaa.md',
    ]);
  });
});

// ─── Errors ─────────────────────────────────────────────────────────────

describe('cmdThread — errors', () => {
  it('seed not found anywhere → error mentioning both folders', () => {
    setupIdentity('bob');
    expect(() => cmdThread(input())).toThrowError(
      /not found in inbox or archive/
    );
  });

  it('invalid filename grammar', () => {
    setupIdentity('bob');
    expect(() => cmdThread(input({ filename: 'garbage' }))).toThrowError(
      /invalid filename/
    );
  });

  it('missing identity folder errors with mkdir hint', () => {
    expect(() => cmdThread(input({ recipient: 'ghost' }))).toThrowError(
      /(agent|identity) folder missing/
    );
  });
});

// ─── Positional disambiguation ──────────────────────────────────────────

describe('splitThreadPositionals', () => {
  it.each([
    [[], { }],
    [['1714826789010-aaaaaa.md'], { filename: '1714826789010-aaaaaa.md' }],
    [['bob'], { recipient: 'bob' }],
    // brief-017a bug 2: .md suffix wins over strict grammar.
    [['nope.md'], { filename: 'nope.md' }],
    [
      ['bob', '1714826789010-aaaaaa.md'],
      { recipient: 'bob', filename: '1714826789010-aaaaaa.md' },
    ],
  ])('split %j → %j', (args, expected) => {
    expect(splitThreadPositionals(args)).toEqual(expected);
  });

  it('three args → throws', () => {
    expect(() =>
      splitThreadPositionals(['a', 'b', 'c'])
    ).toThrowError(/too many arguments/);
  });
});

// ─── formatThreadLine ───────────────────────────────────────────────────

describe('formatThreadLine', () => {
  it('flat (depth 0): no indent, tab-separated', () => {
    expect(
      formatThreadLine({
        filename: '1714826789010-aaaaaa.md',
        from: 'alice',
        subject: 'hi',
        depth: 0,
      })
    ).toBe('1714826789010-aaaaaa.md\talice\thi');
  });

  it('depth N: 2*N leading spaces', () => {
    expect(
      formatThreadLine({
        filename: '1714826789010-aaaaaa.md',
        from: 'alice',
        subject: 'hi',
        depth: 2,
      })
    ).toBe('    1714826789010-aaaaaa.md\talice\thi');
  });
});
