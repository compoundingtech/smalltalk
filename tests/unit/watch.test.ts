// tests/unit/watch.test.ts — coverage for cmd_watch's pure functions.
//
// Per the brief, real-time tests live in the integration suite. The unit
// tests here exercise resolveWatchSetup, watchTargetDirs, watchReplay, and
// watchPoll directly with controlled state — no timers, no sleeps.

import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  formatWatchLine,
  resolveWatchSetup,
  type WatchInput,
  watchPoll,
  watchReplay,
  watchTargetDirs,
} from '../../src/commands/watch.ts';

let scratch: string;
let coordRoot: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-watch-test-'));
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
  to: string,
  filename: string,
  subject = 'sub'
): void {
  writeFileSync(
    join(coordRoot, to, 'inbox', filename),
    `---\nfrom: alice\nsubject: ${subject}\n---\nbody\n`
  );
}

function input(overrides: Partial<WatchInput> = {}): WatchInput {
  return {
    env: {} as NodeJS.ProcessEnv,
    coordRoot,
    ...overrides,
  };
}

// ─── resolveWatchSetup — flag validation + mode resolution ──────────────

describe('resolveWatchSetup — flag validation', () => {
  it('rejects negative --interval', () => {
    setupIdentity('bob');
    expect(() =>
      resolveWatchSetup(input({ recipient: 'bob', intervalMs: -1 }))
    ).toThrowError(/--interval must be a non-negative integer/);
  });

  it('rejects non-integer --interval', () => {
    setupIdentity('bob');
    expect(() =>
      resolveWatchSetup(input({ recipient: 'bob', intervalMs: 1.5 }))
    ).toThrowError(/--interval must be a non-negative integer/);
  });

  it('rejects negative --since', () => {
    setupIdentity('bob');
    expect(() =>
      resolveWatchSetup(input({ recipient: 'bob', since: -1 }))
    ).toThrowError(/--since must be a unix-ms integer/);
  });

  it('rejects --since and --since-now together', () => {
    setupIdentity('bob');
    expect(() =>
      resolveWatchSetup(
        input({ recipient: 'bob', since: 0, sinceNow: true })
      )
    ).toThrowError(/--since and --since-now are mutually exclusive/);
  });
});

describe('resolveWatchSetup — per-identity mode', () => {
  it('uses positional recipient', () => {
    setupIdentity('bob');
    const r = resolveWatchSetup(input({ recipient: 'bob' }));
    expect(r.setup.singleId).toBe('bob');
    expect(r.setup.suppressId).toBeUndefined();
  });

  it('errors with mkdir hint for missing identity', () => {
    expect(() =>
      resolveWatchSetup(input({ recipient: 'ghost' }))
    ).toThrowError(/(agent|identity) folder missing/);
  });
});

describe('resolveWatchSetup — default mode (brief-017a)', () => {
  it('no recipient, no --all → singleId = $COORD_IDENTITY (own inbox)', () => {
    setupIdentity('bob');
    const r = resolveWatchSetup(
      input({ env: { COORD_IDENTITY: 'bob' } as NodeJS.ProcessEnv })
    );
    expect(r.setup.singleId).toBe('bob');
    expect(r.setup.suppressId).toBeUndefined();
  });

  it('default mode honors --from override over env', () => {
    setupIdentity('alice');
    const r = resolveWatchSetup(
      input({
        fromExplicit: 'alice',
        env: { COORD_IDENTITY: 'bob' } as NodeJS.ProcessEnv,
      })
    );
    expect(r.setup.singleId).toBe('alice');
  });

  it('errors with the standard identity-required message when neither is set', () => {
    expect(() => resolveWatchSetup(input())).toThrowError(
      /COORD_IDENTITY/
    );
  });
});

describe('resolveWatchSetup — --all (cross-tree)', () => {
  it('--all + --from → suppressId set, singleId undefined', () => {
    setupIdentity('bob');
    const r = resolveWatchSetup(input({ all: true, fromExplicit: 'bob' }));
    expect(r.setup.singleId).toBeUndefined();
    expect(r.setup.suppressId).toBe('bob');
  });

  it('--all falls back to $COORD_IDENTITY for the suppression id', () => {
    setupIdentity('bob');
    const r = resolveWatchSetup(
      input({ all: true, env: { COORD_IDENTITY: 'bob' } as NodeJS.ProcessEnv })
    );
    expect(r.setup.suppressId).toBe('bob');
  });

  it('--all errors when neither --from nor $COORD_IDENTITY is set', () => {
    expect(() => resolveWatchSetup(input({ all: true }))).toThrowError(
      /identity required to determine which folder to suppress — set COORD_IDENTITY/
    );
  });

  it('--all combined with a positional recipient → flag conflict', () => {
    setupIdentity('bob');
    expect(() =>
      resolveWatchSetup(input({ recipient: 'bob', all: true }))
    ).toThrowError(/--all and <identity> are mutually exclusive/);
  });
});

describe('resolveWatchSetup — cutoff', () => {
  it('default cutoff is 0 (scan-everything-first)', () => {
    setupIdentity('bob');
    const r = resolveWatchSetup(input({ recipient: 'bob' }));
    expect(r.setup.cutoff).toBe(0);
  });

  it('--since UNIX_MS sets explicit cutoff', () => {
    setupIdentity('bob');
    const r = resolveWatchSetup(
      input({ recipient: 'bob', since: 1714826789020 })
    );
    expect(r.setup.cutoff).toBe(1714826789020);
  });

  it('--since-now uses now() (deterministic via override)', () => {
    setupIdentity('bob');
    const r = resolveWatchSetup(
      input({ recipient: 'bob', sinceNow: true, now: () => 42 })
    );
    expect(r.setup.cutoff).toBe(42);
  });
});

// ─── watchTargetDirs ────────────────────────────────────────────────────

describe('watchTargetDirs — per-identity', () => {
  it('returns the single identity inbox', () => {
    setupIdentity('bob');
    const dirs = watchTargetDirs({
      singleId: 'bob',
      cutoff: 0,
      coordRoot,
    });
    expect(dirs).toEqual([join(coordRoot, 'bob', 'inbox')]);
  });
});

describe('watchTargetDirs — cross-tree', () => {
  it('returns all valid-identity inboxes EXCEPT the suppressed one', () => {
    setupIdentity('bob');
    setupIdentity('alice');
    setupIdentity('myobie');
    const dirs = watchTargetDirs({
      suppressId: 'bob',
      cutoff: 0,
      coordRoot,
    });
    const names = dirs.map((d) => join(d, '..').split('/').pop()).sort();
    expect(names).toEqual(['alice', 'myobie']);
  });

  it('skips folders whose name fails valid_identity (e.g. uppercase)', () => {
    setupIdentity('bob');
    mkdirSync(join(coordRoot, 'BAD', 'inbox'), { recursive: true });
    mkdirSync(join(coordRoot, 'BAD', 'archive'), { recursive: true });
    const dirs = watchTargetDirs({
      suppressId: 'bob',
      cutoff: 0,
      coordRoot,
    });
    expect(dirs).toEqual([]);
  });

  it('returns [] when only own folder exists', () => {
    setupIdentity('bob');
    const dirs = watchTargetDirs({
      suppressId: 'bob',
      cutoff: 0,
      coordRoot,
    });
    expect(dirs).toEqual([]);
  });
});

// ─── watchReplay ────────────────────────────────────────────────────────

describe('watchReplay — per-identity', () => {
  it('emits every existing file at cutoff=0 (scan-then-follow default)', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md');
    writeMsg('bob', '1714826789020-bbbbbb.md');
    const r = watchReplay({ singleId: 'bob', cutoff: 0, coordRoot });
    expect(r.lines.map((l) => l.filename)).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789020-bbbbbb.md',
    ]);
    expect(r.seen.size).toBe(2);
  });

  it('emits only files at or after cutoff with explicit --since', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md');
    writeMsg('bob', '1714826789020-bbbbbb.md');
    writeMsg('bob', '1714826789030-cccccc.md');
    const r = watchReplay({
      singleId: 'bob',
      cutoff: 1714826789020,
      coordRoot,
    });
    expect(r.lines.map((l) => l.filename)).toEqual([
      '1714826789020-bbbbbb.md',
      '1714826789030-cccccc.md',
    ]);
    // seen still contains every valid file (so live mode doesn't re-emit).
    expect(r.seen.size).toBe(3);
  });

  it('--since-now (cutoff in the future) emits nothing but seeds seen', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md');
    const r = watchReplay({
      singleId: 'bob',
      cutoff: 9_999_999_999_999,
      coordRoot,
    });
    expect(r.lines).toEqual([]);
    expect(r.seen.has('1714826789010-aaaaaa.md')).toBe(true);
  });

  it('skips files that fail filename grammar', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md');
    writeFileSync(join(coordRoot, 'bob', 'inbox', 'README'), 'x');
    writeFileSync(join(coordRoot, 'bob', 'inbox', 'notes.md'), 'x');
    const r = watchReplay({ singleId: 'bob', cutoff: 0, coordRoot });
    expect(r.lines.map((l) => l.filename)).toEqual([
      '1714826789010-aaaaaa.md',
    ]);
  });

  it('--with-subject attaches the parsed subject', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'hi there');
    const r = watchReplay({
      singleId: 'bob',
      cutoff: 0,
      coordRoot,
      withSubject: true,
    });
    expect(r.lines).toEqual([
      {
        filename: '1714826789010-aaaaaa.md',
        identity: 'bob',
        subject: 'hi there',
      },
    ]);
  });

  it('--with-subject for files without frontmatter → empty subject', () => {
    setupIdentity('bob');
    writeFileSync(
      join(coordRoot, 'bob', 'inbox', '1714826789010-aaaaaa.md'),
      'no frontmatter\n'
    );
    const r = watchReplay({
      singleId: 'bob',
      cutoff: 0,
      coordRoot,
      withSubject: true,
    });
    expect(r.lines).toEqual([
      { filename: '1714826789010-aaaaaa.md', identity: 'bob', subject: '' },
    ]);
  });
});

describe('watchReplay — cross-tree', () => {
  it('emits files from EVERY non-suppressed inbox', () => {
    setupIdentity('bob');
    setupIdentity('alice');
    setupIdentity('myobie');
    writeMsg('alice', '1714826789010-aaaaaa.md');
    writeMsg('myobie', '1714826789020-bbbbbb.md');
    writeMsg('bob', '1714826789030-cccccc.md');
    const r = watchReplay({
      suppressId: 'bob',
      cutoff: 0,
      coordRoot,
    });
    const names = r.lines.map((l) => l.filename).sort();
    expect(names).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789020-bbbbbb.md',
    ]);
  });
});

// ─── watchPoll ──────────────────────────────────────────────────────────

describe('watchPoll', () => {
  it('first poll emits nothing if seen contains the existing files', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md');
    const seen = new Set(['1714826789010-aaaaaa.md']);
    const r = watchPoll(
      { singleId: 'bob', cutoff: 0, coordRoot },
      seen
    );
    expect(r.lines).toEqual([]);
  });

  it('emits files that arrive AFTER replay (i.e. not in seen)', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md'); // existing
    const replay = watchReplay({
      singleId: 'bob',
      cutoff: 0,
      coordRoot,
    });
    expect(replay.lines).toHaveLength(1);

    // Now a new file arrives.
    writeMsg('bob', '1714826789020-bbbbbb.md');
    const r = watchPoll(
      { singleId: 'bob', cutoff: 0, coordRoot },
      replay.seen
    );
    expect(r.lines.map((l) => l.filename)).toEqual([
      '1714826789020-bbbbbb.md',
    ]);
  });

  it('seen-set updates after each poll: a file is emitted at most once', () => {
    setupIdentity('bob');
    const seen = new Set<string>();
    writeMsg('bob', '1714826789010-aaaaaa.md');
    const r1 = watchPoll(
      { singleId: 'bob', cutoff: 0, coordRoot },
      seen
    );
    expect(r1.lines).toHaveLength(1);
    const r2 = watchPoll(
      { singleId: 'bob', cutoff: 0, coordRoot },
      seen
    );
    expect(r2.lines).toEqual([]);
  });

  it('cross-tree poll: a peer-folder created mid-run becomes watched on the next pass', () => {
    setupIdentity('bob');
    setupIdentity('alice');
    const seen = new Set<string>();
    writeMsg('alice', '1714826789010-aaaaaa.md');
    const r1 = watchPoll(
      { suppressId: 'bob', cutoff: 0, coordRoot },
      seen
    );
    expect(r1.lines.map((l) => l.filename)).toEqual([
      '1714826789010-aaaaaa.md',
    ]);

    // myobie's folder appears AFTER replay started.
    setupIdentity('myobie');
    writeMsg('myobie', '1714826789020-bbbbbb.md');
    const r2 = watchPoll(
      { suppressId: 'bob', cutoff: 0, coordRoot },
      seen
    );
    expect(r2.lines.map((l) => l.filename)).toEqual([
      '1714826789020-bbbbbb.md',
    ]);
  });

  it('does NOT fire on archive moves (watch is inbox-only)', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md');
    const replay = watchReplay({
      singleId: 'bob',
      cutoff: 0,
      coordRoot,
    });
    expect(replay.lines).toHaveLength(1);

    // Move the file to archive (not a new inbox arrival).
    const fs = require('node:fs') as typeof import('node:fs');
    fs.renameSync(
      join(coordRoot, 'bob', 'inbox', '1714826789010-aaaaaa.md'),
      join(coordRoot, 'bob', 'archive', '1714826789010-aaaaaa.md')
    );
    const r = watchPoll(
      { singleId: 'bob', cutoff: 0, coordRoot },
      replay.seen
    );
    expect(r.lines).toEqual([]);
  });

  it('does NOT fire on a status file write (live mode is inbox/*.md only)', () => {
    setupIdentity('bob');
    const replay = watchReplay({
      singleId: 'bob',
      cutoff: 0,
      coordRoot,
    });
    writeFileSync(join(coordRoot, 'bob', 'status'), 'busy\n');
    const r = watchPoll(
      { singleId: 'bob', cutoff: 0, coordRoot },
      replay.seen
    );
    expect(r.lines).toEqual([]);
  });
});

// ─── formatWatchLine ────────────────────────────────────────────────────

describe('formatWatchLine', () => {
  it('plain line: just the filename', () => {
    expect(
      formatWatchLine({
        filename: '1714826789010-aaaaaa.md',
        identity: 'bob',
      })
    ).toBe('1714826789010-aaaaaa.md');
  });

  it('with subject: tab-separated', () => {
    expect(
      formatWatchLine({
        filename: '1714826789010-aaaaaa.md',
        identity: 'bob',
        subject: 'hi',
      })
    ).toBe('1714826789010-aaaaaa.md\thi');
  });

  it('with empty subject: still tab-separated', () => {
    expect(
      formatWatchLine({
        filename: '1714826789010-aaaaaa.md',
        identity: 'bob',
        subject: '',
      })
    ).toBe('1714826789010-aaaaaa.md\t');
  });
});
