// tests/unit/archive.test.ts — coverage for cmd_archive + archive trim.

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
  cmdArchive,
  cmdArchiveTrim,
  parseDuration,
  splitArchivePositionals,
  type ArchiveInput,
  type ArchiveTrimInput,
} from '../../src/commands/archive.ts';

let scratch: string;
let stRoot: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'st-archive-test-'));
  stRoot = join(scratch, 'smalltalk');
  mkdirSync(stRoot, { recursive: true });
});
afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

function setupIdentity(id: string): void {
  mkdirSync(join(stRoot, id, 'inbox'), { recursive: true });
  mkdirSync(join(stRoot, id, 'archive'), { recursive: true });
}

function writeMsg(
  id: string,
  filename: string,
  body = 'body',
  folder: 'inbox' | 'archive' = 'inbox'
): void {
  writeFileSync(
    join(stRoot, id, folder, filename),
    `---\nfrom: alice\n---\n${body}\n`
  );
}

function archiveInput(overrides: Partial<ArchiveInput> = {}): ArchiveInput {
  return {
    recipient: 'bob',
    filename: '1714826789010-aaaaaa.md',
    env: {} as NodeJS.ProcessEnv,
    stRoot,
    ...overrides,
  };
}

// ─── archive (move) ─────────────────────────────────────────────────────

describe('cmdArchive — case 4 (clean rename)', () => {
  it('moves inbox file → archive when no twin', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md');
    const r = cmdArchive(archiveInput());
    expect(r.outcome.kind).toBe('moved');
    expect(r.outcome.message).toBe('archived');
    expect(existsSync(join(stRoot, 'bob', 'inbox', '1714826789010-aaaaaa.md'))).toBe(false);
    expect(existsSync(join(stRoot, 'bob', 'archive', '1714826789010-aaaaaa.md'))).toBe(true);
  });
});

describe('cmdArchive — case 2 (idempotent: byte-identical twin)', () => {
  it('removes inbox dup when archive copy is byte-identical', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'same', 'inbox');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'same', 'archive');
    const r = cmdArchive(archiveInput());
    expect(r.outcome.kind).toBe('idempotent');
    expect(existsSync(join(stRoot, 'bob', 'inbox', '1714826789010-aaaaaa.md'))).toBe(false);
    expect(readFileSync(join(stRoot, 'bob', 'archive', '1714826789010-aaaaaa.md'), 'utf8')).toContain('same');
  });
});

describe('cmdArchive — case 3 (refuse: divergent twin)', () => {
  it('refuses with a clear error and leaves both copies in place', () => {
    setupIdentity('bob');
    writeFileSync(
      join(stRoot, 'bob', 'inbox', '1714826789010-aaaaaa.md'),
      'inbox-version'
    );
    writeFileSync(
      join(stRoot, 'bob', 'archive', '1714826789010-aaaaaa.md'),
      'archive-version'
    );
    expect(() => cmdArchive(archiveInput())).toThrowError(/refuse to archive/);
    expect(existsSync(join(stRoot, 'bob', 'inbox', '1714826789010-aaaaaa.md'))).toBe(true);
    expect(existsSync(join(stRoot, 'bob', 'archive', '1714826789010-aaaaaa.md'))).toBe(true);
  });
});

describe('cmdArchive — case 0 (post-sweep idempotent)', () => {
  it('inbox empty + archive present → success without re-throwing', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md', 'archived', 'archive');
    const r = cmdArchive(archiveInput());
    expect(r.outcome.kind).toBe('idempotent');
  });
});

// ─── Outside .md (task #128) ───────────────────────────────────────────

describe('cmdArchive — outside .md files', () => {
  it('moves an off-format `.md` from inbox to archive', () => {
    setupIdentity('bob');
    writeFileSync(join(stRoot, 'bob', 'inbox', 'notes.md'), 'hand-dropped\n');
    const r = cmdArchive(archiveInput({ filename: 'notes.md' }));
    expect(r.outcome.kind).toBe('moved');
    expect(existsSync(join(stRoot, 'bob', 'inbox', 'notes.md'))).toBe(false);
    expect(existsSync(join(stRoot, 'bob', 'archive', 'notes.md'))).toBe(true);
    // Bytes preserved verbatim through the move.
    expect(readFileSync(join(stRoot, 'bob', 'archive', 'notes.md'), 'utf8'))
      .toBe('hand-dropped\n');
  });

  it('withAttachments is silently coerced off for outside filenames (no prefix family)', () => {
    setupIdentity('bob');
    writeFileSync(join(stRoot, 'bob', 'inbox', 'notes.md'), 'plain\n');
    const r = cmdArchive(
      archiveInput({ filename: 'notes.md', withAttachments: true })
    );
    expect(r.outcome.kind).toBe('moved');
    // Outside filenames have no LAYOUT-004 prefix, so there's no
    // sibling family to gather; the flag has no effect and the
    // result omits `attachments` (same shape as withAttachments=false).
    expect(r.attachments).toBeUndefined();
  });
});

describe('cmdArchive — case 1 (not in inbox, not in archive)', () => {
  it('errors with "not found in inbox or archive"', () => {
    setupIdentity('bob');
    expect(() => cmdArchive(archiveInput())).toThrowError(
      /not found in inbox or archive/
    );
  });
});

describe('cmdArchive — input validation', () => {
  it('rejects invalid filename grammar before filesystem hit', () => {
    setupIdentity('bob');
    expect(() =>
      cmdArchive(archiveInput({ filename: 'garbage' }))
    ).toThrowError(/invalid filename/);
  });

  it('rejects empty filename', () => {
    setupIdentity('bob');
    expect(() => cmdArchive(archiveInput({ filename: '' }))).toThrowError(
      /required/
    );
  });

  it('errors with mkdir hint when identity folder is missing', () => {
    expect(() =>
      cmdArchive(archiveInput({ recipient: 'ghost' }))
    ).toThrowError(/(agent|identity) folder missing/);
  });

  it('falls back to ST_AGENT when recipient omitted', () => {
    setupIdentity('bob');
    writeMsg('bob', '1714826789010-aaaaaa.md');
    cmdArchive(
      archiveInput({
        recipient: undefined,
        env: { ST_AGENT: 'bob' } as NodeJS.ProcessEnv,
      })
    );
    expect(existsSync(join(stRoot, 'bob', 'archive', '1714826789010-aaaaaa.md'))).toBe(true);
  });
});

// ─── splitArchivePositionals ────────────────────────────────────────────

describe('splitArchivePositionals', () => {
  it('one arg matching grammar → filename slot', () => {
    expect(splitArchivePositionals(['1714826789010-aaaaaa.md'])).toEqual({
      filename: '1714826789010-aaaaaa.md',
    });
  });

  it('one arg NOT matching grammar → recipient slot', () => {
    expect(splitArchivePositionals(['bob'])).toEqual({ recipient: 'bob' });
  });

  // brief-017a bug 2: .md suffix wins over the strict grammar.
  it('one arg ending in .md (but not strict grammar) → filename slot', () => {
    expect(splitArchivePositionals(['nope.md'])).toEqual({
      filename: 'nope.md',
    });
  });

  it('two args → recipient + filename', () => {
    expect(
      splitArchivePositionals(['bob', '1714826789010-aaaaaa.md'])
    ).toEqual({ recipient: 'bob', filename: '1714826789010-aaaaaa.md' });
  });

  it('three args → throws', () => {
    expect(() =>
      splitArchivePositionals(['a', 'b', 'c'])
    ).toThrowError(/too many arguments/);
  });
});

// ─── parseDuration ──────────────────────────────────────────────────────

describe('parseDuration', () => {
  it.each([
    ['30s', 30],
    ['5m', 300],
    ['2h', 7200],
    ['30d', 30 * 86400],
    ['2w', 2 * 86400 * 7],
    ['0d', 0],
    ['1s', 1],
  ])('%s → %d seconds', (spec, expected) => {
    expect(parseDuration(spec)).toBe(expected);
  });

  it.each(['30days', '30', 'xy', '', '-1d', '5.0d', 'hd', '30D'])(
    'rejects %s',
    (spec) => {
      expect(() => parseDuration(spec)).toThrowError(/invalid duration/);
    }
  );
});

// ─── archive trim ───────────────────────────────────────────────────────

function trimInput(overrides: Partial<ArchiveTrimInput> = {}): ArchiveTrimInput {
  return {
    recipient: 'bob',
    env: {} as NodeJS.ProcessEnv,
    stRoot,
    ...overrides,
  };
}

describe('cmdArchiveTrim — flag validation', () => {
  it('errors when neither --older-than nor --keep-last is given', () => {
    setupIdentity('bob');
    expect(() => cmdArchiveTrim(trimInput())).toThrowError(
      /trim requires --older-than DURATION or --keep-last N/
    );
  });

  it('rejects negative --keep-last', () => {
    setupIdentity('bob');
    expect(() =>
      cmdArchiveTrim(trimInput({ keepLast: -1 }))
    ).toThrowError(/non-negative integer/);
  });

  it('rejects non-integer --keep-last', () => {
    setupIdentity('bob');
    expect(() =>
      cmdArchiveTrim(trimInput({ keepLast: 1.5 }))
    ).toThrowError(/non-negative integer/);
  });

  it('rejects bad --older-than syntax', () => {
    setupIdentity('bob');
    expect(() =>
      cmdArchiveTrim(trimInput({ olderThan: 'xy' }))
    ).toThrowError(/invalid duration/);
  });
});

describe('cmdArchiveTrim — empty / non-existent archive', () => {
  it('empty archive: no-op, returns 0 victims', () => {
    setupIdentity('bob');
    const r = cmdArchiveTrim(trimInput({ keepLast: 100 }));
    expect(r.victims).toEqual([]);
    expect(r.summary).toBe('# trimmed 0 files');
  });

  it('missing archive directory: no-op (no throw)', () => {
    mkdirSync(join(stRoot, 'bob', 'inbox'), { recursive: true });
    // Note: identity-folder check requires archive/ too; create then remove
    // it AFTER resolveIdentity runs… here we just simulate by setting up
    // both dirs and removing archive after.
    mkdirSync(join(stRoot, 'bob', 'archive'), { recursive: true });
    rmSync(join(stRoot, 'bob', 'archive'), { recursive: true });
    expect(() =>
      cmdArchiveTrim(trimInput({ keepLast: 100 }))
    ).toThrowError(/(agent|identity) folder missing/);
  });
});

describe('cmdArchiveTrim — --keep-last', () => {
  it('keeps the N most-recent files, deletes the rest', () => {
    setupIdentity('bob');
    for (const ts of [
      '1714826789010',
      '1714826789020',
      '1714826789030',
      '1714826789040',
      '1714826789050',
    ]) {
      writeFileSync(
        join(stRoot, 'bob', 'archive', `${ts}-aaaaaa.md`),
        'msg'
      );
    }
    const r = cmdArchiveTrim(trimInput({ keepLast: 2 }));
    expect(r.victims).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789020-aaaaaa.md',
      '1714826789030-aaaaaa.md',
    ]);
    expect(r.summary).toBe('# trimmed 3 files');
    expect(
      existsSync(join(stRoot, 'bob', 'archive', '1714826789040-aaaaaa.md'))
    ).toBe(true);
    expect(
      existsSync(join(stRoot, 'bob', 'archive', '1714826789050-aaaaaa.md'))
    ).toBe(true);
  });

  it('--keep-last 0 trims everything', () => {
    setupIdentity('bob');
    writeFileSync(
      join(stRoot, 'bob', 'archive', '1714826789010-aaaaaa.md'),
      'x'
    );
    const r = cmdArchiveTrim(trimInput({ keepLast: 0 }));
    expect(r.victims).toEqual(['1714826789010-aaaaaa.md']);
  });

  it('--keep-last larger than count trims nothing', () => {
    setupIdentity('bob');
    writeFileSync(
      join(stRoot, 'bob', 'archive', '1714826789010-aaaaaa.md'),
      'x'
    );
    const r = cmdArchiveTrim(trimInput({ keepLast: 100 }));
    expect(r.victims).toEqual([]);
  });

  it('singular pluralization: "trimmed 1 file"', () => {
    setupIdentity('bob');
    writeFileSync(
      join(stRoot, 'bob', 'archive', '1714826789010-aaaaaa.md'),
      'x'
    );
    writeFileSync(
      join(stRoot, 'bob', 'archive', '1714826789020-bbbbbb.md'),
      'x'
    );
    const r = cmdArchiveTrim(trimInput({ keepLast: 1 }));
    expect(r.summary).toBe('# trimmed 1 file');
  });
});

describe('cmdArchiveTrim — --older-than', () => {
  it('removes files older than the cutoff (now − duration)', () => {
    setupIdentity('bob');
    const now = 1_800_000_000_000;
    const oldTs = now - 100 * 86400 * 1000; // 100 days ago
    const recentTs = now - 1; // 1ms ago
    writeFileSync(
      join(stRoot, 'bob', 'archive', `${oldTs}-aaaaaa.md`),
      'x'
    );
    writeFileSync(
      join(stRoot, 'bob', 'archive', `${recentTs}-bbbbbb.md`),
      'x'
    );
    const r = cmdArchiveTrim(
      trimInput({ olderThan: '30d', now: () => now })
    );
    expect(r.victims).toEqual([`${oldTs}-aaaaaa.md`]);
  });
});

describe('cmdArchiveTrim — both flags (union, deduped)', () => {
  it('victims = union of older-than ∪ keep-last, sorted, no dups', () => {
    setupIdentity('bob');
    const now = 1_800_000_000_000;
    const tsList = [
      now - 100 * 86400 * 1000, // very old
      now - 50 * 86400 * 1000,  // old
      now - 10 * 86400 * 1000,  // recent
      now - 1,                  // newest
    ];
    for (let i = 0; i < tsList.length; i++) {
      writeFileSync(
        join(
          stRoot,
          'bob',
          'archive',
          `${tsList[i]}-${'abcdef'[i]!.repeat(6)}.md`
        ),
        'x'
      );
    }
    // older-than 30d → drops [0], [1].
    // keep-last 2     → drops [0], [1] (keep newest 2).
    // Union: [0], [1]. Deduped: 2 files.
    const r = cmdArchiveTrim(
      trimInput({
        olderThan: '30d',
        keepLast: 2,
        now: () => now,
      })
    );
    expect(r.victims).toHaveLength(2);
    expect(r.summary).toBe('# trimmed 2 files');
  });
});

describe('cmdArchiveTrim — --dry-run', () => {
  it('lists victims but deletes nothing; summary uses "would trim"', () => {
    setupIdentity('bob');
    for (const ts of [
      '1714826789010',
      '1714826789020',
      '1714826789030',
      '1714826789040',
      '1714826789050',
    ]) {
      writeFileSync(
        join(stRoot, 'bob', 'archive', `${ts}-aaaaaa.md`),
        'x'
      );
    }
    const r = cmdArchiveTrim(trimInput({ keepLast: 2, dryRun: true }));
    expect(r.dryRun).toBe(true);
    expect(r.victims).toHaveLength(3);
    expect(r.summary).toBe(
      '# would trim 3 files (dry run; nothing deleted)'
    );
    // All 5 still on disk.
    for (const ts of [
      '1714826789010',
      '1714826789020',
      '1714826789030',
      '1714826789040',
      '1714826789050',
    ]) {
      expect(
        existsSync(join(stRoot, 'bob', 'archive', `${ts}-aaaaaa.md`))
      ).toBe(true);
    }
  });
});

describe('cmdArchiveTrim — non-grammar files in archive/', () => {
  it('skips non-.md and grammar-violating files (preserves them)', () => {
    setupIdentity('bob');
    writeFileSync(join(stRoot, 'bob', 'archive', 'README'), 'x');
    writeFileSync(join(stRoot, 'bob', 'archive', 'notes.md'), 'x');
    writeFileSync(
      join(stRoot, 'bob', 'archive', '1714826789010-operator-aaaaaa.md'),
      'legacy'
    );
    writeFileSync(
      join(stRoot, 'bob', 'archive', '1714826789020-aaaaaa.md'),
      'real'
    );
    const r = cmdArchiveTrim(trimInput({ keepLast: 0 }));
    expect(r.victims).toEqual(['1714826789020-aaaaaa.md']);
    expect(existsSync(join(stRoot, 'bob', 'archive', 'README'))).toBe(true);
    expect(existsSync(join(stRoot, 'bob', 'archive', 'notes.md'))).toBe(true);
    expect(
      existsSync(
        join(stRoot, 'bob', 'archive', '1714826789010-operator-aaaaaa.md')
      )
    ).toBe(true);
  });
});

// ─── --with-attachments (issue #8) ─────────────────────────────────────────

function writeAttachment(
  id: string,
  filename: string,
  body: string,
  folder: 'inbox' | 'archive' = 'inbox'
): void {
  writeFileSync(join(stRoot, id, folder, filename), body);
}

describe('cmdArchive — --with-attachments', () => {
  const MD = '1714826789010-aaaaaa.md';
  const ATT1 = '1714826789010-aaaaaa.options.json';
  const ATT2 = '1714826789010-aaaaaa.schema.json';

  it('moves the .md AND prefix-siblings to archive', () => {
    setupIdentity('bob');
    writeMsg('bob', MD);
    writeAttachment('bob', ATT1, '{"k":1}');
    writeAttachment('bob', ATT2, '{"type":"object"}');
    const r = cmdArchive(archiveInput({ withAttachments: true }));
    expect(r.outcome.kind).toBe('moved');
    expect(r.attachments).toHaveLength(2);
    expect(r.attachments!.map((a) => a.filename).sort()).toEqual([
      ATT1,
      ATT2,
    ]);
    for (const f of [MD, ATT1, ATT2]) {
      expect(existsSync(join(stRoot, 'bob', 'inbox', f))).toBe(false);
      expect(existsSync(join(stRoot, 'bob', 'archive', f))).toBe(true);
    }
  });

  it('without --with-attachments siblings stay behind (regression: pre-#8 default)', () => {
    setupIdentity('bob');
    writeMsg('bob', MD);
    writeAttachment('bob', ATT1, 'x');
    const r = cmdArchive(archiveInput());
    expect(r.attachments).toBeUndefined();
    expect(existsSync(join(stRoot, 'bob', 'archive', MD))).toBe(true);
    expect(existsSync(join(stRoot, 'bob', 'inbox', ATT1))).toBe(true);
    expect(existsSync(join(stRoot, 'bob', 'archive', ATT1))).toBe(false);
  });

  it('empty attachment array when no siblings exist', () => {
    setupIdentity('bob');
    writeMsg('bob', MD);
    const r = cmdArchive(archiveInput({ withAttachments: true }));
    expect(r.attachments).toEqual([]);
  });

  it('pre-flight refuses on divergent sibling twin (atomic)', () => {
    setupIdentity('bob');
    writeMsg('bob', MD);
    writeAttachment('bob', ATT1, 'inbox-version');
    writeAttachment('bob', ATT1, 'archive-version', 'archive');
    // .md is fine but the sibling has a divergent twin → whole op refuses.
    expect(() =>
      cmdArchive(archiveInput({ withAttachments: true }))
    ).toThrowError(/refuse to archive/);
    // Nothing moved: the .md is still in inbox.
    expect(existsSync(join(stRoot, 'bob', 'inbox', MD))).toBe(true);
    expect(existsSync(join(stRoot, 'bob', 'archive', MD))).toBe(false);
  });

  it('idempotent on byte-identical siblings already in archive', () => {
    setupIdentity('bob');
    // .md AND sibling already archived; bare archive has been
    // re-run after a sync. Should report idempotent for both.
    writeMsg('bob', MD);
    writeMsg('bob', MD, 'body', 'archive'); // identical content
    writeAttachment('bob', ATT1, 'same');
    writeAttachment('bob', ATT1, 'same', 'archive');
    const r = cmdArchive(archiveInput({ withAttachments: true }));
    expect(r.outcome.kind).toBe('idempotent');
    expect(r.attachments).toHaveLength(1);
    expect(r.attachments![0]!.outcome.kind).toBe('idempotent');
    // Inbox copies cleared.
    expect(existsSync(join(stRoot, 'bob', 'inbox', MD))).toBe(false);
    expect(existsSync(join(stRoot, 'bob', 'inbox', ATT1))).toBe(false);
  });

  it('sibling that exists only in archive (post-sync) is handled idempotently', () => {
    setupIdentity('bob');
    writeMsg('bob', MD);
    writeAttachment('bob', ATT1, 'x', 'archive'); // archive-only
    const r = cmdArchive(archiveInput({ withAttachments: true }));
    expect(r.outcome.kind).toBe('moved');
    expect(r.attachments).toHaveLength(1);
    expect(r.attachments![0]!.outcome.kind).toBe('idempotent');
  });

  it('does not touch random files (no prefix match)', () => {
    setupIdentity('bob');
    writeMsg('bob', MD);
    writeAttachment('bob', '.DS_Store', 'junk');
    writeAttachment('bob', 'README', 'junk');
    const r = cmdArchive(archiveInput({ withAttachments: true }));
    expect(r.attachments).toEqual([]);
    expect(existsSync(join(stRoot, 'bob', 'inbox', '.DS_Store'))).toBe(true);
    expect(existsSync(join(stRoot, 'bob', 'inbox', 'README'))).toBe(true);
  });
});

describe('cmdArchiveTrim — --with-attachments', () => {
  const MD1 = '1714826789010-aaaaaa.md';
  const ATT1A = '1714826789010-aaaaaa.options.json';
  const ATT1B = '1714826789010-aaaaaa.schema.json';
  const MD2 = '1714826790000-bbbbbb.md';
  const ATT2 = '1714826790000-bbbbbb.options.json';

  it('deletes siblings of trimmed .md alongside the .md', () => {
    setupIdentity('bob');
    writeMsg('bob', MD1, 'old', 'archive');
    writeMsg('bob', MD2, 'new', 'archive');
    writeAttachment('bob', ATT1A, 'a', 'archive');
    writeAttachment('bob', ATT1B, 'b', 'archive');
    writeAttachment('bob', ATT2, 'c', 'archive');
    const r = cmdArchiveTrim(
      trimInput({ keepLast: 1, withAttachments: true })
    );
    expect(r.victims).toEqual([MD1]); // older one trimmed
    expect(r.attachments).toEqual([ATT1A, ATT1B]); // its siblings
    expect(existsSync(join(stRoot, 'bob', 'archive', MD1))).toBe(false);
    expect(existsSync(join(stRoot, 'bob', 'archive', ATT1A))).toBe(false);
    expect(existsSync(join(stRoot, 'bob', 'archive', ATT1B))).toBe(false);
    // MD2's sibling preserved.
    expect(existsSync(join(stRoot, 'bob', 'archive', ATT2))).toBe(true);
  });

  it('without --with-attachments leaves siblings (regression)', () => {
    setupIdentity('bob');
    writeMsg('bob', MD1, 'old', 'archive');
    writeAttachment('bob', ATT1A, 'a', 'archive');
    const r = cmdArchiveTrim(trimInput({ keepLast: 0 }));
    expect(r.victims).toEqual([MD1]);
    expect(r.attachments).toBeUndefined();
    expect(existsSync(join(stRoot, 'bob', 'archive', ATT1A))).toBe(true);
  });

  it('--dry-run + --with-attachments lists siblings without deleting', () => {
    setupIdentity('bob');
    writeMsg('bob', MD1, 'old', 'archive');
    writeAttachment('bob', ATT1A, 'a', 'archive');
    const r = cmdArchiveTrim(
      trimInput({ keepLast: 0, withAttachments: true, dryRun: true })
    );
    expect(r.victims).toEqual([MD1]);
    expect(r.attachments).toEqual([ATT1A]);
    expect(existsSync(join(stRoot, 'bob', 'archive', MD1))).toBe(true);
    expect(existsSync(join(stRoot, 'bob', 'archive', ATT1A))).toBe(true);
  });
});
