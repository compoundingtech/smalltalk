// tests/unit/common.test.ts — exhaustive coverage of src/common.ts.
//
// Brief-006 task 3 designates this as the test-coverage template for the
// rest of the port: 50+ cases, every helper, every documented edge case.
// Each describe() block targets one helper.

import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  _resetLegacyEnvFallbackWarnings,
  archiveDir,
  assertIdentityFolderExists,
  CROCKFORD_BASE32,
  stConfig,
  stConfigFrom,
  stRoot,
  stRootFrom,
  emitFrontmatter,
  ensureIdentityDirs,
  filenameTimestamp,
  genFilename,
  identityDir,
  inboxDir,
  msNow,
  namePath,
  parseFrontmatter,
  pluralize,
  prefixOf,
  rand6,
  RESERVED_NAMES,
  resolveIdentity,
  rfc3339FromMs,
  safeAtomicWrite,
  stConfig,
  stConfigFrom,
  STATES,
  statusPath,
  sweep,
  hasByteIdenticalArchiveTwin,
  rootShapeWarning,
  validDeliverableFilename,
  validFilename,
  validIdentity,
  validOutsideFilename,
  yamlQuote,
} from '../../src/common.ts';

// ─── Per-test scratch dirs ──────────────────────────────────────────────

let scratch: string;
let stRootDir: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'st-common-test-'));
  stRootDir = join(scratch, 'smalltalk');
  mkdirSync(stRootDir, { recursive: true });
});

afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

// Helper: write a hosted-identity skeleton at $root/<id>/{inbox,archive}.
function setupIdentity(root: string, id: string): void {
  mkdirSync(join(root, id, 'inbox'), { recursive: true });
  mkdirSync(join(root, id, 'archive'), { recursive: true });
}

// ─── Constants ──────────────────────────────────────────────────────────

describe('constants', () => {
  it('CROCKFORD_BASE32 is 32 chars long, no i/l/o/u', () => {
    expect(CROCKFORD_BASE32).toHaveLength(32);
    for (const bad of ['i', 'l', 'o', 'u']) {
      expect(CROCKFORD_BASE32.includes(bad)).toBe(false);
    }
    expect(CROCKFORD_BASE32).toMatch(/^[0-9a-z]{32}$/);
  });

  it('STATES enumerates exactly the LAYOUT states', () => {
    // brief-022: `unknown` joins the four LAYOUT states. It's a derived
    // value (mtime staleness fallback), not user-settable.
    // brief-029: `away` is the fifth settable state for "present but
    // not actively engaged" (distinct from `busy`).
    expect([...STATES]).toEqual([
      'offline',
      'available',
      'busy',
      'away',
      'dnd',
      'unknown',
    ]);
  });

  it('RESERVED_NAMES covers folders, sidecars, state words, and verb names', () => {
    for (const name of [
      'inbox',
      'archive',
      'status',
      'name',
      'offline',
      'available',
      'busy',
      'away', // brief-029
      'dnd',
      'unknown', // brief-022
      'members',
      'overview',
    ]) {
      expect(RESERVED_NAMES.includes(name)).toBe(true);
    }
    // Names that look reserved but are not.
    expect(RESERVED_NAMES.includes('outbox')).toBe(false);
  });

  it('validIdentity rejects `away` (reserved per brief-029)', () => {
    // Same defense as the other state words — an identity literally
    // named `away` would collide with `st status alice --set away`
    // tokenization at the CLI layer.
    expect(validIdentity('away')).toBe(false);
  });
});

// ─── Pluralize ──────────────────────────────────────────────────────────

describe('pluralize', () => {
  it('returns singular for n=1', () => {
    expect(pluralize(1, 'file', 'files')).toBe('file');
  });
  it('returns plural for n=0', () => {
    expect(pluralize(0, 'file', 'files')).toBe('files');
  });
  it('returns plural for n=2', () => {
    expect(pluralize(2, 'file', 'files')).toBe('files');
  });
});

// ─── Time helpers ───────────────────────────────────────────────────────

describe('msNow', () => {
  it('returns 13-digit current ms for the next ~250 years', () => {
    const v = msNow();
    expect(Number.isInteger(v)).toBe(true);
    expect(String(v)).toMatch(/^[0-9]{13}$/);
  });

  it('two consecutive calls are non-decreasing', () => {
    const a = msNow();
    const b = msNow();
    expect(b).toBeGreaterThanOrEqual(a);
  });
});

describe('rfc3339FromMs', () => {
  it('rfc3339FromMs(0) is exactly the epoch ISO string', () => {
    expect(rfc3339FromMs(0)).toBe('1970-01-01T00:00:00.000Z');
  });

  it('rfc3339FromMs(msNow()) is parseable RFC3339 with ms precision', () => {
    const s = rfc3339FromMs(msNow());
    expect(s).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/);
    expect(Number.isFinite(Date.parse(s))).toBe(true);
  });
});

// ─── rand6 + genFilename + validFilename ────────────────────────────────

describe('rand6', () => {
  it('returns 6 chars from the Crockford alphabet', () => {
    for (let i = 0; i < 16; i++) {
      const r = rand6();
      expect(r).toHaveLength(6);
      for (const ch of r) {
        expect(CROCKFORD_BASE32.includes(ch)).toBe(true);
      }
    }
  });
});

describe('genFilename', () => {
  it('produces a parse-able filename matching the LAYOUT grammar', () => {
    const name = genFilename();
    expect(validFilename(name)).toBe(true);
  });

  it('two consecutive calls produce distinct names', () => {
    const a = genFilename();
    const b = genFilename();
    expect(a).not.toBe(b);
  });

  it('the leading <unix-ms> prefix matches msNow at call time', () => {
    const before = msNow();
    const name = genFilename();
    const after = msNow();
    const ts = filenameTimestamp(name);
    expect(ts).toBeGreaterThanOrEqual(before);
    expect(ts).toBeLessThanOrEqual(after);
  });
});

describe('validFilename', () => {
  it('accepts a canonical name', () => {
    expect(validFilename('1714826789012-x9k4mz.md')).toBe(true);
  });

  it('rejects: missing .md', () => {
    expect(validFilename('1714826789012-x9k4mz')).toBe(false);
  });

  it('rejects: wrong .md (e.g. .txt)', () => {
    expect(validFilename('1714826789012-x9k4mz.txt')).toBe(false);
  });

  it('rejects: 12-digit ts', () => {
    expect(validFilename('171482678901-x9k4mz.md')).toBe(false);
  });

  it('rejects: 14-digit ts', () => {
    expect(validFilename('17148267890123-x9k4mz.md')).toBe(false);
  });

  it('rejects: 5-char rand', () => {
    expect(validFilename('1714826789012-x9k4m.md')).toBe(false);
  });

  it('rejects: 7-char rand', () => {
    expect(validFilename('1714826789012-x9k4mzz.md')).toBe(false);
  });

  it('rejects: uppercase in rand', () => {
    expect(validFilename('1714826789012-X9K4MZ.md')).toBe(false);
  });

  it('rejects: empty string', () => {
    expect(validFilename('')).toBe(false);
  });

  it('rejects: just .md', () => {
    expect(validFilename('.md')).toBe(false);
  });

  it('rejects: leading whitespace', () => {
    expect(validFilename(' 1714826789012-x9k4mz.md')).toBe(false);
  });

  it('rejects: trailing newline', () => {
    expect(validFilename('1714826789012-x9k4mz.md\n')).toBe(false);
  });

  it('rejects: legacy 3-segment (<ts>-<machine>-<rand>.md)', () => {
    expect(validFilename('1714826789012-operator-x9k4mz.md')).toBe(false);
  });
});

describe('validOutsideFilename (task #128)', () => {
  it('accepts a plain `.md` basename that is not the canonical grammar', () => {
    expect(validOutsideFilename('notes.md')).toBe(true);
    expect(validOutsideFilename('hello-world.md')).toBe(true);
    expect(validOutsideFilename('outsider.md')).toBe(true);
  });

  it('accepts the legacy 3-segment shape (LAYOUT-004 no longer matches it)', () => {
    // Not our canonical grammar; legitimately hand-dropped or from an
    // older peer that predates the rename.
    expect(validOutsideFilename('1714826789012-operator-x9k4mz.md')).toBe(true);
  });

  it('rejects: canonical LAYOUT-004 grammar (that is a real message, not outside)', () => {
    expect(validOutsideFilename('1714826789012-x9k4mz.md')).toBe(false);
  });

  it('rejects: attachment sidecar of a canonical .md', () => {
    // `<unix-ms>-<r6>.<anything>.md` is a sibling of `<unix-ms>-<r6>.md`
    // and belongs to its family — the canonical .md is responsible.
    expect(validOutsideFilename('1714826789012-x9k4mz.subject.md')).toBe(false);
    expect(validOutsideFilename('1714826789012-x9k4mz.body.md')).toBe(false);
  });

  it('rejects: non-.md files', () => {
    expect(validOutsideFilename('README')).toBe(false);
    expect(validOutsideFilename('notes.txt')).toBe(false);
    expect(validOutsideFilename('data.json')).toBe(false);
  });

  it('rejects: bare .md', () => {
    expect(validOutsideFilename('.md')).toBe(false);
  });

  it('rejects: dotfile / hidden .md', () => {
    expect(validOutsideFilename('.hidden.md')).toBe(false);
    expect(validOutsideFilename('.DS_Store.md')).toBe(false);
  });

  it('rejects: path traversal / paths with separators', () => {
    expect(validOutsideFilename('../escape.md')).toBe(false);
    expect(validOutsideFilename('sub/dir.md')).toBe(false);
    expect(validOutsideFilename('a\\b.md')).toBe(false);
  });

  it('rejects: empty string', () => {
    expect(validOutsideFilename('')).toBe(false);
  });
});

describe('validDeliverableFilename (task #128)', () => {
  it('is the union of validFilename and validOutsideFilename', () => {
    expect(validDeliverableFilename('1714826789012-x9k4mz.md')).toBe(true);
    expect(validDeliverableFilename('notes.md')).toBe(true);
    expect(validDeliverableFilename('README')).toBe(false);
    expect(validDeliverableFilename('../escape.md')).toBe(false);
    expect(validDeliverableFilename('1714826789012-x9k4mz.body.md')).toBe(false);
  });
});

describe('filenameTimestamp', () => {
  it('extracts the <unix-ms> prefix as a number', () => {
    expect(filenameTimestamp('1714826789012-x9k4mz.md')).toBe(1714826789012);
  });

  it('throws on an invalid filename', () => {
    expect(() => filenameTimestamp('garbage')).toThrowError(/invalid filename/);
  });
});

// ─── validIdentity ──────────────────────────────────────────────────────

describe('validIdentity', () => {
  it('accepts simple lowercase names', () => {
    expect(validIdentity('alice')).toBe(true);
    expect(validIdentity('a')).toBe(true);
    expect(validIdentity('a1')).toBe(true);
    expect(validIdentity('1a')).toBe(true);
  });

  it('accepts internal hyphens', () => {
    expect(validIdentity('pty-relay-claude')).toBe(true);
  });

  it('accepts internal periods (issue #1: dotted hierarchy)', () => {
    expect(validIdentity('orchestrator.session-1')).toBe(true);
    expect(validIdentity('a.b.c')).toBe(true);
    expect(validIdentity('parent.child.grandchild')).toBe(true);
  });

  it('accepts long names (issue #1: 32-char cap removed)', () => {
    // A delegation-path-as-identity wants `<persona>.<26-char-ulid>` or
    // deeper, which blows the old 32-char cap immediately. The cap was
    // defensive, not load-bearing — POSIX paths accept far longer, and
    // there's no smalltalk invariant that depends on a length bound.
    expect(validIdentity('a'.repeat(64))).toBe(true);
    expect(validIdentity('persona.01arz3ndektsv4rrffq69g5fav')).toBe(true);
    expect(validIdentity('a'.repeat(255))).toBe(true);
  });

  it('rejects empty', () => {
    expect(validIdentity('')).toBe(false);
  });

  it('rejects uppercase', () => {
    expect(validIdentity('Alice')).toBe(false);
    expect(validIdentity('alicE')).toBe(false);
  });

  it('rejects leading hyphen', () => {
    expect(validIdentity('-alice')).toBe(false);
  });

  it('rejects trailing hyphen', () => {
    expect(validIdentity('alice-')).toBe(false);
  });

  it('rejects whitespace', () => {
    expect(validIdentity('al ice')).toBe(false);
    expect(validIdentity(' alice')).toBe(false);
    expect(validIdentity('alice ')).toBe(false);
  });

  it('rejects leading period', () => {
    expect(validIdentity('.alice')).toBe(false);
  });

  it('rejects trailing period', () => {
    expect(validIdentity('alice.')).toBe(false);
  });

  it('rejects path separators (slashes are NOT supported — issue #1)', () => {
    expect(validIdentity('alice/bob')).toBe(false);
    expect(validIdentity('../etc')).toBe(false);
  });

  it('rejects non-[a-z0-9.-] chars', () => {
    expect(validIdentity('alice_bob')).toBe(false);
    expect(validIdentity('aliçe')).toBe(false);
  });

  it.each(RESERVED_NAMES)('rejects reserved name: %s', (name) => {
    expect(validIdentity(name)).toBe(false);
  });

  it('outbox is NOT reserved (free for use)', () => {
    expect(validIdentity('outbox')).toBe(true);
  });
});

// ─── resolveIdentity ────────────────────────────────────────────────────

describe('resolveIdentity', () => {
  it('uses the explicit arg when provided (env ignored)', () => {
    setupIdentity(stRootDir, 'alice');
    setupIdentity(stRootDir, 'bob');
    expect(
      resolveIdentity({
        explicit: 'alice',
        env: { ST_AGENT: 'bob' } as NodeJS.ProcessEnv,
        stRoot: stRootDir,
      })
    ).toBe('alice');
  });

  it('falls back to ST_AGENT when no explicit arg', () => {
    setupIdentity(stRootDir, 'bob');
    expect(
      resolveIdentity({
        env: { ST_AGENT: 'bob' } as NodeJS.ProcessEnv,
        stRoot: stRootDir,
      })
    ).toBe('bob');
  });

  it('errors loudly mentioning ST_AGENT when neither is set', () => {
    setupIdentity(stRootDir, 'bob');
    expect(() =>
      resolveIdentity({
        env: {} as NodeJS.ProcessEnv,
        stRoot: stRootDir,
      })
    ).toThrowError(/ST_AGENT/);
  });

  it('errors when the resolved identity is not valid', () => {
    expect(() =>
      resolveIdentity({
        explicit: 'INVALID',
        env: {} as NodeJS.ProcessEnv,
        stRoot: stRootDir,
      })
    ).toThrowError(/invalid (agent name|identity)/);
  });

  it('errors when the identity is a reserved name', () => {
    expect(() =>
      resolveIdentity({
        explicit: 'inbox',
        env: {} as NodeJS.ProcessEnv,
        stRoot: stRootDir,
      })
    ).toThrowError(/invalid (agent name|identity)/);
  });

  it('errors with the mkdir hint when the folder is missing', () => {
    expect(() =>
      resolveIdentity({
        explicit: 'ghost',
        env: {} as NodeJS.ProcessEnv,
        stRoot: stRootDir,
      })
    ).toThrowError(/(agent|identity) folder missing for ghost/);
  });

  it('errors with the mkdir hint when only inbox exists', () => {
    mkdirSync(join(stRootDir, 'half', 'inbox'), { recursive: true });
    expect(() =>
      resolveIdentity({
        explicit: 'half',
        env: {} as NodeJS.ProcessEnv,
        stRoot: stRootDir,
      })
    ).toThrowError(/(agent|identity) folder missing/);
  });

  // ─── First-run polish: lazy bootstrap on $ST_AGENT ────────────

  it('auto-creates <id>/{inbox,archive} when $ST_AGENT resolves and folders are missing', () => {
    // Fresh root, no pre-existing identity. Should NOT throw — first
    // command for a brand-new identity bootstraps the folders on
    // demand.
    expect(
      resolveIdentity({
        env: { ST_AGENT: 'newperson' } as NodeJS.ProcessEnv,
        stRoot: stRootDir,
      })
    ).toBe('newperson');
    expect(
      isDirSync(join(stRootDir, 'newperson', 'inbox'))
    ).toBe(true);
    expect(
      isDirSync(join(stRootDir, 'newperson', 'archive'))
    ).toBe(true);
  });

  it('auto-create is idempotent — second call with the same identity is a no-op', () => {
    resolveIdentity({
      env: { ST_AGENT: 'idempot' } as NodeJS.ProcessEnv,
      stRoot: stRootDir,
    });
    // Second call works the same way; no error, no double-mkdir issue.
    expect(() =>
      resolveIdentity({
        env: { ST_AGENT: 'idempot' } as NodeJS.ProcessEnv,
        stRoot: stRootDir,
      })
    ).not.toThrow();
  });

  it('--from <other> still errors if <other> is missing (anti-impersonation)', () => {
    // The lazy bootstrap is for "you, claiming to be you" via
    // $ST_AGENT. Explicit <other> stays strict.
    expect(() =>
      resolveIdentity({
        explicit: 'ghost',
        env: { ST_AGENT: 'whoever' } as NodeJS.ProcessEnv,
        stRoot: stRootDir,
      })
    ).toThrowError(/(agent|identity) folder missing for ghost/);
  });

  it('--from <other> matching an existing identity still works after self bootstrap', () => {
    setupIdentity(stRootDir, 'bob');
    expect(
      resolveIdentity({
        explicit: 'bob',
        env: { ST_AGENT: 'newperson' } as NodeJS.ProcessEnv,
        stRoot: stRootDir,
      })
    ).toBe('bob');
  });

  // ─── Lenient policy: cross-identity reads tolerate partial peer trees ──

  it("policy: 'lenient' accepts <id> when ONLY inbox/ exists", () => {
    mkdirSync(join(stRootDir, 'partial', 'inbox'), { recursive: true });
    expect(
      resolveIdentity({
        explicit: 'partial',
        env: {} as NodeJS.ProcessEnv,
        stRoot: stRootDir,
        policy: 'lenient',
      })
    ).toBe('partial');
    // partial's archive was NOT created — lenient is observe-only.
    expect(isDirSync(join(stRootDir, 'partial', 'archive'))).toBe(false);
  });

  it("policy: 'lenient' accepts <id> when ONLY archive/ exists", () => {
    mkdirSync(join(stRootDir, 'archived', 'archive'), { recursive: true });
    expect(
      resolveIdentity({
        explicit: 'archived',
        env: {} as NodeJS.ProcessEnv,
        stRoot: stRootDir,
        policy: 'lenient',
      })
    ).toBe('archived');
    expect(isDirSync(join(stRootDir, 'archived', 'inbox'))).toBe(false);
  });

  it("policy: 'lenient' still throws when NEITHER inbox nor archive exists", () => {
    expect(() =>
      resolveIdentity({
        explicit: 'phantom',
        env: {} as NodeJS.ProcessEnv,
        stRoot: stRootDir,
        policy: 'lenient',
      })
    ).toThrowError(/(agent|identity) folder missing for phantom/);
  });

  it("policy: 'lenient' is ignored on the $ST_AGENT path (still auto-creates)", () => {
    // Same `policy: 'lenient'` call but no `explicit` — bootstrap
    // wins because we never reach the policy branch.
    resolveIdentity({
      env: { ST_AGENT: 'ownbootstrap' } as NodeJS.ProcessEnv,
      stRoot: stRootDir,
      policy: 'lenient',
    });
    expect(isDirSync(join(stRootDir, 'ownbootstrap', 'inbox'))).toBe(true);
    expect(isDirSync(join(stRootDir, 'ownbootstrap', 'archive'))).toBe(
      true
    );
  });
});

function isDirSync(p: string): boolean {
  try {
    return statSync(p).isDirectory();
  } catch {
    return false;
  }
}

// ─── Path helpers ───────────────────────────────────────────────────────

describe('path helpers', () => {
  it('stRootFrom honors $ST_ROOT when set', () => {
    expect(stRootFrom({ ST_ROOT: '/tmp/x' } as NodeJS.ProcessEnv)).toBe(
      '/tmp/x'
    );
  });

  it('stRootFrom falls back to ~/.local/state/smalltalk', () => {
    const v = stRootFrom({} as NodeJS.ProcessEnv);
    expect(v.endsWith('.local/state/smalltalk')).toBe(true);
  });

  it('stConfigFrom honors $ST_CONFIG when set', () => {
    expect(
      stConfigFrom({ ST_CONFIG: '/tmp/st-config' } as NodeJS.ProcessEnv)
    ).toBe('/tmp/st-config');
  });

  it('stConfigFrom falls back to ~/.config/smalltalk when $ST_CONFIG unset', () => {
    const v = stConfigFrom({ HOME: scratch } as NodeJS.ProcessEnv);
    expect(v.endsWith('.config/smalltalk')).toBe(true);
  });

  it('stRoot()/stConfig() read live process.env', () => {
    // Smoke test: just confirm the wrappers don't throw and return strings.
    expect(typeof stRoot()).toBe('string');
    expect(typeof stConfig()).toBe('string');
  });

  it('identityDir / inboxDir / archiveDir / statusPath / namePath compose correctly', () => {
    const root = '/r';
    expect(identityDir('alice', root)).toBe('/r/alice');
    expect(inboxDir('alice', root)).toBe('/r/alice/inbox');
    expect(archiveDir('alice', root)).toBe('/r/alice/archive');
    expect(statusPath('alice', root)).toBe('/r/alice/status');
    expect(namePath('alice', root)).toBe('/r/alice/name');
  });
});

// ─── Folder existence ───────────────────────────────────────────────────

describe('assertIdentityFolderExists', () => {
  it('passes when both inbox and archive exist', () => {
    setupIdentity(stRootDir, 'alice');
    expect(() => assertIdentityFolderExists('alice', stRootDir)).not.toThrow();
  });

  it('throws naming the path when inbox is missing', () => {
    mkdirSync(join(stRootDir, 'alice', 'archive'), { recursive: true });
    expect(() =>
      assertIdentityFolderExists('alice', stRootDir)
    ).toThrowError(
      /(agent|identity) folder missing for alice — create it: mkdir -p \$ST_ROOT\/alice\/{inbox,archive}/
    );
  });

  it('throws when archive is missing', () => {
    mkdirSync(join(stRootDir, 'alice', 'inbox'), { recursive: true });
    expect(() => assertIdentityFolderExists('alice', stRootDir)).toThrowError(
      /(agent|identity) folder missing/
    );
  });

  it('throws when nothing exists', () => {
    expect(() => assertIdentityFolderExists('ghost', stRootDir)).toThrow();
  });
});

describe('ensureIdentityDirs', () => {
  it('creates inbox and archive (idempotent)', () => {
    ensureIdentityDirs('alice', stRootDir);
    expect(() => assertIdentityFolderExists('alice', stRootDir)).not.toThrow();
    // Re-running is a no-op.
    ensureIdentityDirs('alice', stRootDir);
    expect(() => assertIdentityFolderExists('alice', stRootDir)).not.toThrow();
  });
});

// ─── Frontmatter parsing ────────────────────────────────────────────────

describe('parseFrontmatter', () => {
  it('empty file → fm={}, body=""', () => {
    expect(parseFrontmatter('')).toEqual({ fm: {}, body: '' });
  });

  it('plain text (no fences) → fm={}, body=text', () => {
    expect(parseFrontmatter('hello world')).toEqual({
      fm: {},
      body: 'hello world',
    });
  });

  it('empty fenced frontmatter → fm={}, body after fence', () => {
    expect(parseFrontmatter('---\n---\nhello')).toEqual({
      fm: {},
      body: 'hello',
    });
  });

  it('single-key frontmatter', () => {
    const r = parseFrontmatter('---\nfrom: alice\n---\nbody');
    expect(r.fm).toEqual({ from: 'alice' });
    expect(r.body).toBe('body');
  });

  it('multiple keys', () => {
    const r = parseFrontmatter(
      '---\nfrom: alice\nsubject: hi\nin-reply-to: 1714826789012-x9k4mz.md\n---\nbody'
    );
    expect(r.fm).toEqual({
      from: 'alice',
      subject: 'hi',
      'in-reply-to': '1714826789012-x9k4mz.md',
    });
    expect(r.body).toBe('body');
  });

  it('double-quoted value: quotes stripped', () => {
    expect(parseFrontmatter('---\nsubject: "hello"\n---\n').fm).toEqual({
      subject: 'hello',
    });
  });

  it('single-quoted value: quotes stripped', () => {
    expect(parseFrontmatter("---\nsubject: 'hello'\n---\n").fm).toEqual({
      subject: 'hello',
    });
  });

  it('double-quoted with embedded escaped quote', () => {
    expect(
      parseFrontmatter('---\nsubject: "with \\"quotes\\""\n---\n').fm
    ).toEqual({ subject: 'with "quotes"' });
  });

  it('double-quoted with embedded escaped backslash', () => {
    expect(parseFrontmatter('---\nsubject: "back\\\\slash"\n---\n').fm).toEqual(
      { subject: 'back\\slash' }
    );
  });

  it('double-quoted with embedded escaped newline', () => {
    expect(parseFrontmatter('---\nsubject: "a\\nb"\n---\n').fm).toEqual({
      subject: 'a\nb',
    });
  });

  it('unterminated opening fence → fm={}, body=whole text', () => {
    const text = '---\nfrom: alice\nno close';
    expect(parseFrontmatter(text)).toEqual({ fm: {}, body: text });
  });

  it('blank lines inside frontmatter are tolerated', () => {
    const r = parseFrontmatter('---\n\nfrom: alice\n\n---\nbody');
    expect(r.fm).toEqual({ from: 'alice' });
    expect(r.body).toBe('body');
  });

  it('# comment lines inside frontmatter are ignored', () => {
    const r = parseFrontmatter('---\n# this is a comment\nfrom: alice\n---\nbody');
    expect(r.fm).toEqual({ from: 'alice' });
  });

  it('tab indentation in frontmatter values is tolerated', () => {
    const r = parseFrontmatter('---\n\tfrom:\talice\n---\n');
    expect(r.fm).toEqual({ from: 'alice' });
  });

  it('body containing a "---" line is not confused with the closing fence', () => {
    const r = parseFrontmatter('---\nfrom: alice\n---\nbefore\n---\nafter');
    expect(r.fm).toEqual({ from: 'alice' });
    expect(r.body).toBe('before\n---\nafter');
  });

  it('list value passes through as the literal raw string', () => {
    const r = parseFrontmatter('---\ntags: [a, b]\n---\n');
    expect(r.fm).toEqual({ tags: '[a, b]' });
  });

  it('numeric value passes through as a string', () => {
    const r = parseFrontmatter('---\npriority: 5\n---\n');
    expect(r.fm).toEqual({ priority: '5' });
  });

  it('value containing ":" survives (split is on first colon only)', () => {
    const r = parseFrontmatter('---\nsubject: re: hello\n---\n');
    expect(r.fm).toEqual({ subject: 're: hello' });
  });

  it('preserves trailing newline in body', () => {
    expect(parseFrontmatter('---\nfrom: a\n---\nbody\n').body).toBe('body\n');
  });

  it('open fence not on line 1 → not frontmatter', () => {
    expect(parseFrontmatter('  \n---\nfrom: a\n---\nbody')).toEqual({
      fm: {},
      body: '  \n---\nfrom: a\n---\nbody',
    });
  });
});

// ─── Frontmatter emission + roundtrip ───────────────────────────────────

describe('emitFrontmatter', () => {
  it('empty fm → "---\\n---\\n<body>"', () => {
    expect(emitFrontmatter({}, '')).toBe('---\n---\n');
    expect(emitFrontmatter({}, 'hello')).toBe('---\n---\nhello');
  });

  it('identifier-shape values emit unquoted', () => {
    expect(emitFrontmatter({ from: 'alice' }, '')).toBe(
      '---\nfrom: alice\n---\n'
    );
  });

  it('values with colons / hashes / spaces are quoted', () => {
    expect(emitFrontmatter({ subject: 'hello: world' }, '')).toBe(
      '---\nsubject: "hello: world"\n---\n'
    );
  });

  it('arrays of plain-scalar identifiers emit unquoted inside the flow list', () => {
    // brief-017a bug 4: plain-scalar tags must not be double-quoted
    // so parseTagsScalar round-trips cleanly. Strings that need
    // escaping fall back to yamlQuote().
    expect(emitFrontmatter({ tags: ['a', 'b'] }, '')).toBe(
      '---\ntags: [a, b]\n---\n'
    );
  });

  it('arrays mix plain + quoted elements as needed', () => {
    expect(emitFrontmatter({ tags: ['ok', 'has space'] }, '')).toBe(
      '---\ntags: [ok, "has space"]\n---\n'
    );
  });

  it('numbers emit as plain scalars', () => {
    expect(emitFrontmatter({ priority: 5 }, '')).toBe(
      '---\npriority: 5\n---\n'
    );
  });

  it('booleans emit as plain scalars', () => {
    expect(emitFrontmatter({ ok: true }, '')).toBe('---\nok: true\n---\n');
  });

  it('undefined values are skipped', () => {
    expect(emitFrontmatter({ from: 'a', skip: undefined }, '')).toBe(
      '---\nfrom: a\n---\n'
    );
  });

  it.each([
    ['plain', 'hello'],
    ['embedded "', 'with "quotes"'],
    ['embedded \\', 'back\\slash'],
    ['embedded newline', 'first\nsecond'],
    ['leading whitespace', '   leading'],
    ['embedded colon', 'with: colon'],
    ['embedded hash', 'with #hash'],
    ['embedded tab', 'with\ttab'],
  ])('roundtrip survives: %s', (_label, v) => {
    const text = emitFrontmatter({ subject: v }, '');
    expect(parseFrontmatter(text).fm.subject).toBe(v);
  });
});

// ─── yamlQuote ──────────────────────────────────────────────────────────

describe('yamlQuote', () => {
  it('empty string → ""', () => {
    expect(yamlQuote('')).toBe('""');
  });

  it('simple identifier', () => {
    expect(yamlQuote('hello')).toBe('"hello"');
  });

  it('embedded " is escaped', () => {
    expect(yamlQuote('a"b')).toBe('"a\\"b"');
  });

  it('embedded \\ is escaped', () => {
    expect(yamlQuote('a\\b')).toBe('"a\\\\b"');
  });

  it('embedded newline is escaped', () => {
    expect(yamlQuote('a\nb')).toBe('"a\\nb"');
  });

  it('escape order: \\ first, then "', () => {
    // For input `\"`, expected: backslash-escape both chars: `\\` then `\"`.
    expect(yamlQuote('\\"')).toBe('"\\\\\\""');
  });
});

// ─── safeAtomicWrite ────────────────────────────────────────────────────

describe('safeAtomicWrite', () => {
  it('writes a new file at the given path', () => {
    const f = join(scratch, 'out.md');
    safeAtomicWrite(f, 'hello');
    expect(readFileSync(f, 'utf8')).toBe('hello');
  });

  it('refuses to overwrite an existing file (throws)', () => {
    const f = join(scratch, 'already.md');
    writeFileSync(f, 'first');
    expect(() => safeAtomicWrite(f, 'second')).toThrow();
    expect(readFileSync(f, 'utf8')).toBe('first');
  });

  it('throws if parent directory is missing', () => {
    const f = join(scratch, 'nope', 'out.md');
    expect(() => safeAtomicWrite(f, 'x')).toThrow();
  });

  it('binary bytes survive (Buffer input)', () => {
    const f = join(scratch, 'bin.md');
    const buf = Buffer.from([0x00, 0x01, 0xff, 0xfe]);
    safeAtomicWrite(f, buf);
    expect(readFileSync(f).equals(buf)).toBe(true);
  });

  it('unicode survives', () => {
    const f = join(scratch, 'uni.md');
    safeAtomicWrite(f, 'héllo 世界 🦀');
    expect(readFileSync(f, 'utf8')).toBe('héllo 世界 🦀');
  });
});

// ─── sweep ──────────────────────────────────────────────────────────────

describe('sweep', () => {
  it('empty $ST_ROOT (no children) → { removed: 0 }', () => {
    expect(sweep(stRootDir)).toEqual({ removed: 0 });
  });

  it('inbox file with no archive twin → preserved', () => {
    setupIdentity(stRootDir, 'bob');
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(join(stRootDir, 'bob', 'inbox', f), 'live');
    expect(sweep(stRootDir)).toEqual({ removed: 0 });
    expect(readFileSync(join(stRootDir, 'bob', 'inbox', f), 'utf8')).toBe(
      'live'
    );
  });

  it('inbox + identical archive twin → inbox copy removed, removed=1', () => {
    setupIdentity(stRootDir, 'bob');
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(join(stRootDir, 'bob', 'inbox', f), 'same');
    writeFileSync(join(stRootDir, 'bob', 'archive', f), 'same');
    expect(sweep(stRootDir)).toEqual({ removed: 1 });
    expect(
      require('node:fs').existsSync(join(stRootDir, 'bob', 'inbox', f))
    ).toBe(false);
    expect(
      readFileSync(join(stRootDir, 'bob', 'archive', f), 'utf8')
    ).toBe('same');
  });

  it('inbox + DIVERGENT archive twin → inbox preserved (skip)', () => {
    setupIdentity(stRootDir, 'bob');
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(join(stRootDir, 'bob', 'inbox', f), 'inbox-version');
    writeFileSync(join(stRootDir, 'bob', 'archive', f), 'archive-version');
    expect(sweep(stRootDir)).toEqual({ removed: 0 });
    expect(
      readFileSync(join(stRootDir, 'bob', 'inbox', f), 'utf8')
    ).toBe('inbox-version');
  });

  it('multiple identities, mixed states, each handled independently', () => {
    setupIdentity(stRootDir, 'alice');
    setupIdentity(stRootDir, 'bob');
    setupIdentity(stRootDir, 'carol');

    // alice: identical pair → swept.
    const f1 = '1714826789010-aaaaaa.md';
    writeFileSync(join(stRootDir, 'alice', 'inbox', f1), 'A');
    writeFileSync(join(stRootDir, 'alice', 'archive', f1), 'A');

    // bob: inbox-only → preserved.
    const f2 = '1714826789020-bbbbbb.md';
    writeFileSync(join(stRootDir, 'bob', 'inbox', f2), 'B');

    // carol: identical pair → swept.
    const f3 = '1714826789030-cccccc.md';
    writeFileSync(join(stRootDir, 'carol', 'inbox', f3), 'C');
    writeFileSync(join(stRootDir, 'carol', 'archive', f3), 'C');

    expect(sweep(stRootDir)).toEqual({ removed: 2 });
  });

  it('identity with no archive/ dir → no-op (no error)', () => {
    mkdirSync(join(stRootDir, 'lonely', 'inbox'), { recursive: true });
    expect(() => sweep(stRootDir)).not.toThrow();
    expect(sweep(stRootDir)).toEqual({ removed: 0 });
  });

  it('archive/ contains non-.md files → ignored', () => {
    setupIdentity(stRootDir, 'bob');
    writeFileSync(join(stRootDir, 'bob', 'archive', 'notes.md'), 'x'); // bad grammar
    writeFileSync(join(stRootDir, 'bob', 'archive', 'README'), 'x');
    expect(sweep(stRootDir)).toEqual({ removed: 0 });
  });

  it('archive has X.md, no <id>/inbox/ dir → no-op', () => {
    mkdirSync(join(stRootDir, 'bob', 'archive'), { recursive: true });
    writeFileSync(
      join(stRootDir, 'bob', 'archive', '1714826789010-aaaaaa.md'),
      'x'
    );
    expect(sweep(stRootDir)).toEqual({ removed: 0 });
  });

  it('idempotent: a second sweep is a no-op', () => {
    setupIdentity(stRootDir, 'bob');
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(join(stRootDir, 'bob', 'inbox', f), 'same');
    writeFileSync(join(stRootDir, 'bob', 'archive', f), 'same');
    expect(sweep(stRootDir)).toEqual({ removed: 1 });
    expect(sweep(stRootDir)).toEqual({ removed: 0 });
  });

  it('returns { removed: 0 } when $ST_ROOT does not exist', () => {
    expect(sweep(join(scratch, 'does-not-exist'))).toEqual({ removed: 0 });
  });

  // ─── Attachment-family sweep (issue #8) ──────────────────────────────
  //
  // When a `.md` AND its prefix-siblings exist in archive byte-identical
  // to inbox copies (the result of `archive --with-attachments` plus a
  // round of rsync), sweep must remove the inbox copies of EVERY family
  // member to keep `rsync` from resurrecting zombies.

  describe('attachment families (issue #8)', () => {
    it('sibling byte-identical pair + matching archive .md → sibling swept', () => {
      setupIdentity(stRootDir, 'bob');
      const md = '1714826789010-aaaaaa.md';
      const att = '1714826789010-aaaaaa.options.json';
      writeFileSync(join(stRootDir, 'bob', 'inbox', md), 'M');
      writeFileSync(join(stRootDir, 'bob', 'archive', md), 'M');
      writeFileSync(join(stRootDir, 'bob', 'inbox', att), '{"k":1}');
      writeFileSync(join(stRootDir, 'bob', 'archive', att), '{"k":1}');
      expect(sweep(stRootDir)).toEqual({ removed: 2 });
      expect(
        require('node:fs').existsSync(join(stRootDir, 'bob', 'inbox', md))
      ).toBe(false);
      expect(
        require('node:fs').existsSync(join(stRootDir, 'bob', 'inbox', att))
      ).toBe(false);
    });

    it('sibling without a matching archive .md → preserved (not st-owned)', () => {
      setupIdentity(stRootDir, 'bob');
      const att = '1714826789010-aaaaaa.options.json';
      writeFileSync(join(stRootDir, 'bob', 'inbox', att), 'X');
      writeFileSync(join(stRootDir, 'bob', 'archive', att), 'X');
      // No matching archive/.md; sweep must not touch this — it's
      // indistinguishable from a random file the user happened to put
      // in both folders.
      expect(sweep(stRootDir)).toEqual({ removed: 0 });
      expect(
        require('node:fs').existsSync(join(stRootDir, 'bob', 'inbox', att))
      ).toBe(true);
    });

    it('divergent sibling pair → preserved (no data loss)', () => {
      setupIdentity(stRootDir, 'bob');
      const md = '1714826789010-aaaaaa.md';
      const att = '1714826789010-aaaaaa.options.json';
      writeFileSync(join(stRootDir, 'bob', 'inbox', md), 'M');
      writeFileSync(join(stRootDir, 'bob', 'archive', md), 'M');
      writeFileSync(join(stRootDir, 'bob', 'inbox', att), 'inbox');
      writeFileSync(join(stRootDir, 'bob', 'archive', att), 'archive');
      // The .md still gets swept; the divergent sibling does NOT.
      expect(sweep(stRootDir)).toEqual({ removed: 1 });
      expect(
        require('node:fs').existsSync(join(stRootDir, 'bob', 'inbox', att))
      ).toBe(true);
    });

    it('random file (no LAYOUT prefix) is always ignored', () => {
      setupIdentity(stRootDir, 'bob');
      writeFileSync(join(stRootDir, 'bob', 'inbox', '.DS_Store'), 'x');
      writeFileSync(join(stRootDir, 'bob', 'archive', '.DS_Store'), 'x');
      expect(sweep(stRootDir)).toEqual({ removed: 0 });
      expect(
        require('node:fs').existsSync(
          join(stRootDir, 'bob', 'inbox', '.DS_Store')
        )
      ).toBe(true);
    });

    it('multiple siblings of one .md → all swept', () => {
      setupIdentity(stRootDir, 'bob');
      const md = '1714826789010-aaaaaa.md';
      const att1 = '1714826789010-aaaaaa.options.json';
      const att2 = '1714826789010-aaaaaa.schema.json';
      for (const f of [md, att1, att2]) {
        writeFileSync(join(stRootDir, 'bob', 'inbox', f), f);
        writeFileSync(join(stRootDir, 'bob', 'archive', f), f);
      }
      expect(sweep(stRootDir)).toEqual({ removed: 3 });
    });
  });
});

// ─── prefixOf ──────────────────────────────────────────────────────────

describe('prefixOf', () => {
  it('extracts the 20-char prefix from a canonical .md', () => {
    expect(prefixOf('1714826789010-aaaaaa.md')).toBe('1714826789010-aaaaaa');
  });

  it('extracts the prefix from a sibling attachment', () => {
    expect(prefixOf('1714826789010-aaaaaa.options.json')).toBe(
      '1714826789010-aaaaaa'
    );
    expect(prefixOf('1714826789010-aaaaaa.schema.json')).toBe(
      '1714826789010-aaaaaa'
    );
  });

  it('returns null when the prefix grammar does not match', () => {
    expect(prefixOf('readme.md')).toBeNull();
    expect(prefixOf('.DS_Store')).toBeNull();
    expect(prefixOf('shortprefix.md')).toBeNull();
  });

  it('returns null when there is no extension at all', () => {
    // Bare prefix isn't a smalltalk file — smalltalk names always carry an
    // extension (`.md` or the attachment suffix).
    expect(prefixOf('1714826789010-aaaaaa')).toBeNull();
  });

  it('matches validFilename grammar exactly (12-digit ts → reject)', () => {
    expect(prefixOf('171482678901-aaaaaa.options.json')).toBeNull(); // 12 digits
    expect(prefixOf('17148267890100-aaaaaa.options.json')).toBeNull(); // 14 digits
  });

  it('matches validFilename: liberal rand6 (i/l/o/u permitted)', () => {
    // We intentionally mirror validFilename's `[0-9a-z]{6}` rather than
    // the strict Crockford alphabet, so a `.md` and its siblings stay
    // associated even on names that drift from the canonical alphabet.
    expect(prefixOf('1714826789010-iiiiii.options.json')).toBe(
      '1714826789010-iiiiii'
    );
  });
});

describe('hasByteIdenticalArchiveTwin', () => {
  const f = '1714826789010-aaaaaa.md';

  it('inbox + byte-identical archive twin → true (resurrected zombie)', () => {
    setupIdentity(stRootDir, 'bob');
    writeFileSync(join(stRootDir, 'bob', 'inbox', f), 'same');
    writeFileSync(join(stRootDir, 'bob', 'archive', f), 'same');
    expect(hasByteIdenticalArchiveTwin(stRootDir, 'bob', f)).toBe(true);
  });

  it('inbox only, no archive twin → false (fresh delivery)', () => {
    setupIdentity(stRootDir, 'bob');
    writeFileSync(join(stRootDir, 'bob', 'inbox', f), 'live');
    expect(hasByteIdenticalArchiveTwin(stRootDir, 'bob', f)).toBe(false);
  });

  it('DIVERGENT pair (inbox edited away from archive) → false', () => {
    setupIdentity(stRootDir, 'bob');
    writeFileSync(join(stRootDir, 'bob', 'inbox', f), 'inbox-version');
    writeFileSync(join(stRootDir, 'bob', 'archive', f), 'archive-version');
    expect(hasByteIdenticalArchiveTwin(stRootDir, 'bob', f)).toBe(false);
  });

  it('archive twin exists but inbox copy gone → false', () => {
    setupIdentity(stRootDir, 'bob');
    writeFileSync(join(stRootDir, 'bob', 'archive', f), 'same');
    expect(hasByteIdenticalArchiveTwin(stRootDir, 'bob', f)).toBe(false);
  });

  it('agrees with sweep: exactly the pair sweep removes reports true', () => {
    setupIdentity(stRootDir, 'bob');
    const twin = '1714826789010-aaaaaa.md';
    const fresh = '1714826789020-bbbbbb.md';
    writeFileSync(join(stRootDir, 'bob', 'inbox', twin), 'x');
    writeFileSync(join(stRootDir, 'bob', 'archive', twin), 'x');
    writeFileSync(join(stRootDir, 'bob', 'inbox', fresh), 'x');
    expect(hasByteIdenticalArchiveTwin(stRootDir, 'bob', twin)).toBe(true);
    expect(hasByteIdenticalArchiveTwin(stRootDir, 'bob', fresh)).toBe(false);
    expect(sweep(stRootDir)).toEqual({ removed: 1 });
  });
});

describe('rootShapeWarning', () => {
  it('missing root → null (other errors own that)', () => {
    expect(rootShapeWarning(join(scratch, 'does-not-exist'))).toBeNull();
  });

  it('empty (new) root → null (no false alarm)', () => {
    expect(rootShapeWarning(stRootDir)).toBeNull();
  });

  it('correct bus root (agent-shaped children, no nesting) → null', () => {
    setupIdentity(stRootDir, 'alice');
    setupIdentity(stRootDir, 'bob');
    writeFileSync(join(stRootDir, 'alice', 'inbox', '1714826789010-aaaaaa.md'), 'x');
    expect(rootShapeWarning(stRootDir)).toBeNull();
  });

  it('TOO SHALLOW: a nested bus tree below the root → WARN naming it', () => {
    // Mimics ST_ROOT=/…/convoy while the bus is /…/convoy/default/smalltalk.
    const busRoot = join(stRootDir, 'default', 'smalltalk');
    setupIdentity(busRoot, 'alice');
    const w = rootShapeWarning(stRootDir);
    expect(w).toContain('too shallow');
    expect(w).toContain(busRoot);
  });

  it('does not false-positive on a real bus root nested under it', () => {
    // The correct root itself must NOT warn even though its agents have
    // inbox/archive subdirs (those are not agent-shaped grandchildren).
    const busRoot = join(stRootDir, 'default', 'smalltalk');
    setupIdentity(busRoot, 'alice');
    setupIdentity(busRoot, 'bob');
    expect(rootShapeWarning(busRoot)).toBeNull();
  });

  it('NON-AGENT-SHAPED: non-empty root with no agent dirs → WARN', () => {
    mkdirSync(join(stRootDir, 'notanagent'), { recursive: true });
    writeFileSync(join(stRootDir, 'stray.txt'), 'x');
    const w = rootShapeWarning(stRootDir);
    expect(w).toContain('no agent-shaped children');
  });
});
