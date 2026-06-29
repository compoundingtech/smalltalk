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
  archiveDir,
  assertIdentityFolderExists,
  CROCKFORD_BASE32,
  coordConfig,
  coordConfigFrom,
  coordRoot,
  coordRootFrom,
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
  STATES,
  statusPath,
  sweep,
  validFilename,
  validIdentity,
  yamlQuote,
} from '../../src/common.ts';

// ─── Per-test scratch dirs ──────────────────────────────────────────────

let scratch: string;
let coordRootDir: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-common-test-'));
  coordRootDir = join(scratch, 'coord');
  mkdirSync(coordRootDir, { recursive: true });
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
    // named `away` would collide with `coord status alice --set away`
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
    expect(validFilename('1714826789012-myobie-x9k4mz.md')).toBe(false);
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
    // there's no coord invariant that depends on a length bound.
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
    setupIdentity(coordRootDir, 'alice');
    setupIdentity(coordRootDir, 'bob');
    expect(
      resolveIdentity({
        explicit: 'alice',
        env: { COORD_IDENTITY: 'bob' } as NodeJS.ProcessEnv,
        coordRoot: coordRootDir,
      })
    ).toBe('alice');
  });

  it('falls back to COORD_IDENTITY when no explicit arg', () => {
    setupIdentity(coordRootDir, 'bob');
    expect(
      resolveIdentity({
        env: { COORD_IDENTITY: 'bob' } as NodeJS.ProcessEnv,
        coordRoot: coordRootDir,
      })
    ).toBe('bob');
  });

  it('errors loudly mentioning COORD_IDENTITY when neither is set', () => {
    setupIdentity(coordRootDir, 'bob');
    expect(() =>
      resolveIdentity({
        env: {} as NodeJS.ProcessEnv,
        coordRoot: coordRootDir,
      })
    ).toThrowError(/COORD_IDENTITY/);
  });

  it('errors when the resolved identity is not valid', () => {
    expect(() =>
      resolveIdentity({
        explicit: 'INVALID',
        env: {} as NodeJS.ProcessEnv,
        coordRoot: coordRootDir,
      })
    ).toThrowError(/invalid (agent name|identity)/);
  });

  it('errors when the identity is a reserved name', () => {
    expect(() =>
      resolveIdentity({
        explicit: 'inbox',
        env: {} as NodeJS.ProcessEnv,
        coordRoot: coordRootDir,
      })
    ).toThrowError(/invalid (agent name|identity)/);
  });

  it('errors with the mkdir hint when the folder is missing', () => {
    expect(() =>
      resolveIdentity({
        explicit: 'ghost',
        env: {} as NodeJS.ProcessEnv,
        coordRoot: coordRootDir,
      })
    ).toThrowError(/(agent|identity) folder missing for ghost/);
  });

  it('errors with the mkdir hint when only inbox exists', () => {
    mkdirSync(join(coordRootDir, 'half', 'inbox'), { recursive: true });
    expect(() =>
      resolveIdentity({
        explicit: 'half',
        env: {} as NodeJS.ProcessEnv,
        coordRoot: coordRootDir,
      })
    ).toThrowError(/(agent|identity) folder missing/);
  });

  // ─── First-run polish: lazy bootstrap on $COORD_IDENTITY ────────────

  it('auto-creates <id>/{inbox,archive} when $COORD_IDENTITY resolves and folders are missing', () => {
    // Fresh root, no pre-existing identity. Should NOT throw — first
    // command for a brand-new identity bootstraps the folders on
    // demand.
    expect(
      resolveIdentity({
        env: { COORD_IDENTITY: 'newperson' } as NodeJS.ProcessEnv,
        coordRoot: coordRootDir,
      })
    ).toBe('newperson');
    expect(
      isDirSync(join(coordRootDir, 'newperson', 'inbox'))
    ).toBe(true);
    expect(
      isDirSync(join(coordRootDir, 'newperson', 'archive'))
    ).toBe(true);
  });

  it('auto-create is idempotent — second call with the same identity is a no-op', () => {
    resolveIdentity({
      env: { COORD_IDENTITY: 'idempot' } as NodeJS.ProcessEnv,
      coordRoot: coordRootDir,
    });
    // Second call works the same way; no error, no double-mkdir issue.
    expect(() =>
      resolveIdentity({
        env: { COORD_IDENTITY: 'idempot' } as NodeJS.ProcessEnv,
        coordRoot: coordRootDir,
      })
    ).not.toThrow();
  });

  it('--from <other> still errors if <other> is missing (anti-impersonation)', () => {
    // The lazy bootstrap is for "you, claiming to be you" via
    // $COORD_IDENTITY. Explicit <other> stays strict.
    expect(() =>
      resolveIdentity({
        explicit: 'ghost',
        env: { COORD_IDENTITY: 'whoever' } as NodeJS.ProcessEnv,
        coordRoot: coordRootDir,
      })
    ).toThrowError(/(agent|identity) folder missing for ghost/);
  });

  it('--from <other> matching an existing identity still works after self bootstrap', () => {
    setupIdentity(coordRootDir, 'bob');
    expect(
      resolveIdentity({
        explicit: 'bob',
        env: { COORD_IDENTITY: 'newperson' } as NodeJS.ProcessEnv,
        coordRoot: coordRootDir,
      })
    ).toBe('bob');
  });

  // ─── Lenient policy: cross-identity reads tolerate partial peer trees ──

  it("policy: 'lenient' accepts <id> when ONLY inbox/ exists", () => {
    mkdirSync(join(coordRootDir, 'partial', 'inbox'), { recursive: true });
    expect(
      resolveIdentity({
        explicit: 'partial',
        env: {} as NodeJS.ProcessEnv,
        coordRoot: coordRootDir,
        policy: 'lenient',
      })
    ).toBe('partial');
    // partial's archive was NOT created — lenient is observe-only.
    expect(isDirSync(join(coordRootDir, 'partial', 'archive'))).toBe(false);
  });

  it("policy: 'lenient' accepts <id> when ONLY archive/ exists", () => {
    mkdirSync(join(coordRootDir, 'archived', 'archive'), { recursive: true });
    expect(
      resolveIdentity({
        explicit: 'archived',
        env: {} as NodeJS.ProcessEnv,
        coordRoot: coordRootDir,
        policy: 'lenient',
      })
    ).toBe('archived');
    expect(isDirSync(join(coordRootDir, 'archived', 'inbox'))).toBe(false);
  });

  it("policy: 'lenient' still throws when NEITHER inbox nor archive exists", () => {
    expect(() =>
      resolveIdentity({
        explicit: 'phantom',
        env: {} as NodeJS.ProcessEnv,
        coordRoot: coordRootDir,
        policy: 'lenient',
      })
    ).toThrowError(/(agent|identity) folder missing for phantom/);
  });

  it("policy: 'lenient' is ignored on the $COORD_IDENTITY path (still auto-creates)", () => {
    // Same `policy: 'lenient'` call but no `explicit` — bootstrap
    // wins because we never reach the policy branch.
    resolveIdentity({
      env: { COORD_IDENTITY: 'ownbootstrap' } as NodeJS.ProcessEnv,
      coordRoot: coordRootDir,
      policy: 'lenient',
    });
    expect(isDirSync(join(coordRootDir, 'ownbootstrap', 'inbox'))).toBe(true);
    expect(isDirSync(join(coordRootDir, 'ownbootstrap', 'archive'))).toBe(
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
  it('coordRootFrom honors $COORD_ROOT when set', () => {
    expect(coordRootFrom({ COORD_ROOT: '/tmp/x' } as NodeJS.ProcessEnv)).toBe(
      '/tmp/x'
    );
  });

  it('coordRootFrom falls back to ~/.local/state/smalltalk', () => {
    const v = coordRootFrom({} as NodeJS.ProcessEnv);
    expect(v.endsWith('.local/state/smalltalk')).toBe(true);
  });

  it('coordConfigFrom honors $COORD_CONFIG', () => {
    expect(
      coordConfigFrom({ COORD_CONFIG: '/tmp/y' } as NodeJS.ProcessEnv)
    ).toBe('/tmp/y');
  });

  it('coordConfigFrom falls back to ~/.config/coord', () => {
    const v = coordConfigFrom({} as NodeJS.ProcessEnv);
    expect(v.endsWith('.config/coord')).toBe(true);
  });

  it('coordRoot()/coordConfig() read live process.env', () => {
    // Smoke test: just confirm the wrappers don't throw and return strings.
    expect(typeof coordRoot()).toBe('string');
    expect(typeof coordConfig()).toBe('string');
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
    setupIdentity(coordRootDir, 'alice');
    expect(() => assertIdentityFolderExists('alice', coordRootDir)).not.toThrow();
  });

  it('throws naming the path when inbox is missing', () => {
    mkdirSync(join(coordRootDir, 'alice', 'archive'), { recursive: true });
    expect(() =>
      assertIdentityFolderExists('alice', coordRootDir)
    ).toThrowError(
      /(agent|identity) folder missing for alice — create it: mkdir -p \$(ST|COORD)_ROOT\/alice\/{inbox,archive}/
    );
  });

  it('throws when archive is missing', () => {
    mkdirSync(join(coordRootDir, 'alice', 'inbox'), { recursive: true });
    expect(() => assertIdentityFolderExists('alice', coordRootDir)).toThrowError(
      /(agent|identity) folder missing/
    );
  });

  it('throws when nothing exists', () => {
    expect(() => assertIdentityFolderExists('ghost', coordRootDir)).toThrow();
  });
});

describe('ensureIdentityDirs', () => {
  it('creates inbox and archive (idempotent)', () => {
    ensureIdentityDirs('alice', coordRootDir);
    expect(() => assertIdentityFolderExists('alice', coordRootDir)).not.toThrow();
    // Re-running is a no-op.
    ensureIdentityDirs('alice', coordRootDir);
    expect(() => assertIdentityFolderExists('alice', coordRootDir)).not.toThrow();
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
  it('empty $COORD_ROOT (no children) → { removed: 0 }', () => {
    expect(sweep(coordRootDir)).toEqual({ removed: 0 });
  });

  it('inbox file with no archive twin → preserved', () => {
    setupIdentity(coordRootDir, 'bob');
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(join(coordRootDir, 'bob', 'inbox', f), 'live');
    expect(sweep(coordRootDir)).toEqual({ removed: 0 });
    expect(readFileSync(join(coordRootDir, 'bob', 'inbox', f), 'utf8')).toBe(
      'live'
    );
  });

  it('inbox + identical archive twin → inbox copy removed, removed=1', () => {
    setupIdentity(coordRootDir, 'bob');
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(join(coordRootDir, 'bob', 'inbox', f), 'same');
    writeFileSync(join(coordRootDir, 'bob', 'archive', f), 'same');
    expect(sweep(coordRootDir)).toEqual({ removed: 1 });
    expect(
      require('node:fs').existsSync(join(coordRootDir, 'bob', 'inbox', f))
    ).toBe(false);
    expect(
      readFileSync(join(coordRootDir, 'bob', 'archive', f), 'utf8')
    ).toBe('same');
  });

  it('inbox + DIVERGENT archive twin → inbox preserved (skip)', () => {
    setupIdentity(coordRootDir, 'bob');
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(join(coordRootDir, 'bob', 'inbox', f), 'inbox-version');
    writeFileSync(join(coordRootDir, 'bob', 'archive', f), 'archive-version');
    expect(sweep(coordRootDir)).toEqual({ removed: 0 });
    expect(
      readFileSync(join(coordRootDir, 'bob', 'inbox', f), 'utf8')
    ).toBe('inbox-version');
  });

  it('multiple identities, mixed states, each handled independently', () => {
    setupIdentity(coordRootDir, 'alice');
    setupIdentity(coordRootDir, 'bob');
    setupIdentity(coordRootDir, 'carol');

    // alice: identical pair → swept.
    const f1 = '1714826789010-aaaaaa.md';
    writeFileSync(join(coordRootDir, 'alice', 'inbox', f1), 'A');
    writeFileSync(join(coordRootDir, 'alice', 'archive', f1), 'A');

    // bob: inbox-only → preserved.
    const f2 = '1714826789020-bbbbbb.md';
    writeFileSync(join(coordRootDir, 'bob', 'inbox', f2), 'B');

    // carol: identical pair → swept.
    const f3 = '1714826789030-cccccc.md';
    writeFileSync(join(coordRootDir, 'carol', 'inbox', f3), 'C');
    writeFileSync(join(coordRootDir, 'carol', 'archive', f3), 'C');

    expect(sweep(coordRootDir)).toEqual({ removed: 2 });
  });

  it('identity with no archive/ dir → no-op (no error)', () => {
    mkdirSync(join(coordRootDir, 'lonely', 'inbox'), { recursive: true });
    expect(() => sweep(coordRootDir)).not.toThrow();
    expect(sweep(coordRootDir)).toEqual({ removed: 0 });
  });

  it('archive/ contains non-.md files → ignored', () => {
    setupIdentity(coordRootDir, 'bob');
    writeFileSync(join(coordRootDir, 'bob', 'archive', 'notes.md'), 'x'); // bad grammar
    writeFileSync(join(coordRootDir, 'bob', 'archive', 'README'), 'x');
    expect(sweep(coordRootDir)).toEqual({ removed: 0 });
  });

  it('archive has X.md, no <id>/inbox/ dir → no-op', () => {
    mkdirSync(join(coordRootDir, 'bob', 'archive'), { recursive: true });
    writeFileSync(
      join(coordRootDir, 'bob', 'archive', '1714826789010-aaaaaa.md'),
      'x'
    );
    expect(sweep(coordRootDir)).toEqual({ removed: 0 });
  });

  it('idempotent: a second sweep is a no-op', () => {
    setupIdentity(coordRootDir, 'bob');
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(join(coordRootDir, 'bob', 'inbox', f), 'same');
    writeFileSync(join(coordRootDir, 'bob', 'archive', f), 'same');
    expect(sweep(coordRootDir)).toEqual({ removed: 1 });
    expect(sweep(coordRootDir)).toEqual({ removed: 0 });
  });

  it('returns { removed: 0 } when $COORD_ROOT does not exist', () => {
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
      setupIdentity(coordRootDir, 'bob');
      const md = '1714826789010-aaaaaa.md';
      const att = '1714826789010-aaaaaa.options.json';
      writeFileSync(join(coordRootDir, 'bob', 'inbox', md), 'M');
      writeFileSync(join(coordRootDir, 'bob', 'archive', md), 'M');
      writeFileSync(join(coordRootDir, 'bob', 'inbox', att), '{"k":1}');
      writeFileSync(join(coordRootDir, 'bob', 'archive', att), '{"k":1}');
      expect(sweep(coordRootDir)).toEqual({ removed: 2 });
      expect(
        require('node:fs').existsSync(join(coordRootDir, 'bob', 'inbox', md))
      ).toBe(false);
      expect(
        require('node:fs').existsSync(join(coordRootDir, 'bob', 'inbox', att))
      ).toBe(false);
    });

    it('sibling without a matching archive .md → preserved (not coord-owned)', () => {
      setupIdentity(coordRootDir, 'bob');
      const att = '1714826789010-aaaaaa.options.json';
      writeFileSync(join(coordRootDir, 'bob', 'inbox', att), 'X');
      writeFileSync(join(coordRootDir, 'bob', 'archive', att), 'X');
      // No matching archive/.md; sweep must not touch this — it's
      // indistinguishable from a random file the user happened to put
      // in both folders.
      expect(sweep(coordRootDir)).toEqual({ removed: 0 });
      expect(
        require('node:fs').existsSync(join(coordRootDir, 'bob', 'inbox', att))
      ).toBe(true);
    });

    it('divergent sibling pair → preserved (no data loss)', () => {
      setupIdentity(coordRootDir, 'bob');
      const md = '1714826789010-aaaaaa.md';
      const att = '1714826789010-aaaaaa.options.json';
      writeFileSync(join(coordRootDir, 'bob', 'inbox', md), 'M');
      writeFileSync(join(coordRootDir, 'bob', 'archive', md), 'M');
      writeFileSync(join(coordRootDir, 'bob', 'inbox', att), 'inbox');
      writeFileSync(join(coordRootDir, 'bob', 'archive', att), 'archive');
      // The .md still gets swept; the divergent sibling does NOT.
      expect(sweep(coordRootDir)).toEqual({ removed: 1 });
      expect(
        require('node:fs').existsSync(join(coordRootDir, 'bob', 'inbox', att))
      ).toBe(true);
    });

    it('random file (no LAYOUT prefix) is always ignored', () => {
      setupIdentity(coordRootDir, 'bob');
      writeFileSync(join(coordRootDir, 'bob', 'inbox', '.DS_Store'), 'x');
      writeFileSync(join(coordRootDir, 'bob', 'archive', '.DS_Store'), 'x');
      expect(sweep(coordRootDir)).toEqual({ removed: 0 });
      expect(
        require('node:fs').existsSync(
          join(coordRootDir, 'bob', 'inbox', '.DS_Store')
        )
      ).toBe(true);
    });

    it('multiple siblings of one .md → all swept', () => {
      setupIdentity(coordRootDir, 'bob');
      const md = '1714826789010-aaaaaa.md';
      const att1 = '1714826789010-aaaaaa.options.json';
      const att2 = '1714826789010-aaaaaa.schema.json';
      for (const f of [md, att1, att2]) {
        writeFileSync(join(coordRootDir, 'bob', 'inbox', f), f);
        writeFileSync(join(coordRootDir, 'bob', 'archive', f), f);
      }
      expect(sweep(coordRootDir)).toEqual({ removed: 3 });
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
    // Bare prefix isn't a coord file — coord names always carry an
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
