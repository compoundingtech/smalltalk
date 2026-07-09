// tests/unit/cli.test.ts — coverage for the dispatcher (runCli).
//
// Exercises argv parsing, dispatch routing, error handling, and the
// universal pre-command sweep — all without spawning a subprocess.

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

import { runCli, type CliContext } from '../../src/cli.ts';

let scratch: string;
let stRoot: string;
let stConfig: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-cli-test-'));
  stRoot = join(scratch, 'coord');
  stConfig = join(scratch, 'config');
  mkdirSync(stRoot, { recursive: true });
  mkdirSync(stConfig, { recursive: true });
});
afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

interface Capture {
  stdout: string;
  stderr: string;
  ctx: CliContext;
}

function makeContext(
  stdin: string | Buffer = '',
  envOverrides: NodeJS.ProcessEnv = {}
): Capture {
  const cap: Capture = {
    stdout: '',
    stderr: '',
    // Filled in below; placeholder.
    ctx: undefined as unknown as CliContext,
  };
  cap.ctx = {
    env: envOverrides,
    stRoot,
    stConfig,
    stdout: (s) => {
      cap.stdout += s;
    },
    stderr: (s) => {
      cap.stderr += s;
    },
    readStdin: async () =>
      typeof stdin === 'string' ? Buffer.from(stdin, 'utf8') : stdin,
  };
  return cap;
}

function setupIdentity(id: string): void {
  mkdirSync(join(stRoot, id, 'inbox'), { recursive: true });
  mkdirSync(join(stRoot, id, 'archive'), { recursive: true });
}

// ─── No args / help ─────────────────────────────────────────────────────

describe('runCli — no args / help', () => {
  it('no args → usage to stderr, exit 2', async () => {
    const cap = makeContext();
    const code = await runCli([], cap.ctx);
    expect(code).toBe(2);
    expect(cap.stdout).toBe('');
    expect(cap.stderr).toContain('usage: st');
  });

  it('help → usage to stdout, exit 0', async () => {
    const cap = makeContext();
    const code = await runCli(['help'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stdout).toContain('usage: st');
  });

  it('--help → usage to stdout, exit 0', async () => {
    const cap = makeContext();
    const code = await runCli(['--help'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stdout).toContain('usage: st');
  });

  it('-h → usage to stdout, exit 0', async () => {
    const cap = makeContext();
    const code = await runCli(['-h'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stdout).toContain('usage: st');
  });
});

// ─── --version ──────────────────────────────────────────────────────────

describe('runCli — --version', () => {
  // `st --version` reads package.json at runtime and prints
  // `<invokedName> <semver>+<short-sha>`, following the same
  // brand-per-name convention as the help banners. The `+<short-sha>`
  // build suffix is dropped gracefully when not a git checkout.
  it('prints "<name> <semver>[+<sha>]" to stdout, exit 0', async () => {
    const cap = makeContext();
    const code = await runCli(['--version'], cap.ctx);
    expect(code).toBe(0);
    // `st X.Y.Z` (optional pre-release), optional `+<short-sha>` build.
    expect(cap.stdout).toMatch(
      /^st \d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9a-f]+)?\n$/
    );
    expect(cap.stderr).toBe('');
  });

  it('includes the +<short-sha> build suffix (running from a git checkout)', async () => {
    // The test suite runs inside the smalltalk git checkout, so the
    // build SHA resolves and must be appended as semver build metadata.
    const cap = makeContext();
    await runCli(['--version'], cap.ctx);
    expect(cap.stdout).toMatch(/^st \d+\.\d+\.\d+\+[0-9a-f]{4,}\n$/);
  });

  it('reflects _ST_INVOKED_AS in the brand', async () => {
    const cap = makeContext('', { _ST_INVOKED_AS: 'coord' });
    const code = await runCli(['--version'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stdout).toMatch(/^coord \d+\.\d+\.\d+/);
  });

  it('mentions --version in the top-level help banner', async () => {
    const cap = makeContext();
    const code = await runCli(['help'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stdout).toContain('--version');
  });
});

// ─── _ST_INVOKED_AS override ────────────────────────────────────────────

describe('runCli — help banner reflects _ST_INVOKED_AS', () => {
  // The bin/ shims export `_ST_INVOKED_AS` from their $0 basename, and
  // the help banner interpolates that name so users see what they
  // typed. When unset (fresh dev shells, direct `node src/cli.ts`
  // invocations, most tests), the banner defaults to `st`.
  it('defaults to `st` when unset', async () => {
    const cap = makeContext('', {});
    const code = await runCli(['help'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stdout).toContain('usage: st ');
    expect(cap.stdout).not.toContain('usage: coord ');
  });

  it('reflects `coord` when the shim exports it', async () => {
    const cap = makeContext('', { _ST_INVOKED_AS: 'coord' });
    const code = await runCli(['help'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stdout).toContain('usage: coord ');
  });

  it('reflects `smalltalk` when the shim exports it', async () => {
    const cap = makeContext('', { _ST_INVOKED_AS: 'smalltalk' });
    const code = await runCli(['help'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stdout).toContain('usage: smalltalk ');
  });

  it('unknown-subcommand error prefix also reflects the invoked name', async () => {
    const cap = makeContext('', { _ST_INVOKED_AS: 'coord' });
    const code = await runCli(['bogus'], cap.ctx);
    expect(code).toBe(2);
    expect(cap.stderr).toContain('coord: unknown subcommand: bogus');
  });

  it('empty _ST_INVOKED_AS falls back to `st`', async () => {
    // Guard against bin/ shim regressions that would export an empty
    // string when $0 basename fails. The helper treats empty the same
    // as unset.
    const cap = makeContext('', { _ST_INVOKED_AS: '' });
    const code = await runCli(['help'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stdout).toContain('usage: st ');
  });
});

// ─── Unknown subcommand ─────────────────────────────────────────────────

describe('runCli — unknown subcommand', () => {
  it('prints "unknown subcommand" + usage on stderr, exit 2', async () => {
    const cap = makeContext();
    const code = await runCli(['bogus'], cap.ctx);
    expect(code).toBe(2);
    expect(cap.stderr).toContain('unknown subcommand: bogus');
    expect(cap.stderr).toContain('usage: st');
  });
});

// ─── Per-command --help ─────────────────────────────────────────────────

describe('runCli — per-command --help / -h prints command-specific usage', () => {
  // Brief-017: message verbs are nested under `st message`; the
  // CLI surfaces them via `st message <verb> --help` (or the `msg`
  // alias). Non-message verbs remain top-level.
  it.each([
    [['message', 'send'], 'usage: st message send'],
    [['message', 'ls'], 'usage: st message ls'],
    [['message', 'read'], 'usage: st message read'],
    [['message', 'archive'], 'usage: st message archive'],
    [['message', 'thread'], 'usage: st message thread'],
    [['msg', 'send'], 'usage: st message send'],
    [['watch'], 'usage: st watch'],
    [['status'], 'usage: st status'],
    [['sync'], 'usage: st sync'],
  ] as const)('%j --help', async (cmd, prefix) => {
    const cap = makeContext();
    const code = await runCli([...cmd, '--help'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stderr).toContain(prefix);
  });

  it.each([
    [['message', 'send'], 'usage: st message send'],
    [['message', 'ls'], 'usage: st message ls'],
  ] as const)('%j -h', async (cmd, prefix) => {
    const cap = makeContext();
    const code = await runCli([...cmd, '-h'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stderr).toContain(prefix);
  });

  it('top-level `st send` (pre-brief-017 flat form) errors with a pointer', async () => {
    const cap = makeContext();
    const code = await runCli(['send', 'bob'], cap.ctx);
    expect(code).toBe(2);
    expect(cap.stderr).toContain('Did you mean `st message send`?');
  });

  it('`st message --help` prints the message-group banner', async () => {
    const cap = makeContext();
    const code = await runCli(['message', '--help'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stdout).toContain('st message <verb>');
  });
});

// ─── Non-sync commands DO NOT presweep (post sweep-as-convergence) ──────

describe('runCli — no inline presweep on non-sync commands', () => {
  it('`ls` shows the zombie inbox copy (no inline sweep)', async () => {
    // Per the new sweep-as-convergence policy, ls does not run a
    // presweep — a byte-identical inbox/archive twin will appear in
    // ls output until lazy-read sweep, `coord sweep`, or a sync runs.
    setupIdentity('bob');
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(join(stRoot, 'bob', 'inbox', f), 'same');
    writeFileSync(join(stRoot, 'bob', 'archive', f), 'same');
    const cap = makeContext();
    const code = await runCli(['message', 'ls', 'bob'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stdout).toContain(f);
    // Zombie still on disk after ls.
    expect(existsSync(join(stRoot, 'bob', 'inbox', f))).toBe(true);
  });

  it('`read` lazy-sweeps the zombie inbox copy (byte-identical twin)', async () => {
    setupIdentity('bob');
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(join(stRoot, 'bob', 'inbox', f), 'same');
    writeFileSync(join(stRoot, 'bob', 'archive', f), 'same');
    const cap = makeContext();
    const code = await runCli(['message', 'read', 'bob', f], cap.ctx);
    expect(code).toBe(0);
    // Lazy-read sweep cleaned up the inbox copy.
    expect(existsSync(join(stRoot, 'bob', 'inbox', f))).toBe(false);
    expect(existsSync(join(stRoot, 'bob', 'archive', f))).toBe(true);
  });
});

// ─── Errors are surfaced as `coord: <msg>` on stderr, exit 1 ────────────

describe('runCli — error formatting', () => {
  it('command throw becomes "coord: <message>" on stderr, exit 1', async () => {
    // No identity context = identity-required error from cmdLs.
    const cap = makeContext();
    const code = await runCli(['message', 'ls'], cap.ctx);
    expect(code).toBe(1);
    expect(cap.stderr).toMatch(/^st: (agent|identity) required/);
  });

  it('unknown flag → exit 1 with "unknown flag" message', async () => {
    setupIdentity('bob');
    const cap = makeContext('', { ST_AGENT: 'bob' });
    const code = await runCli(['message', 'ls', '--bogus'], cap.ctx);
    expect(code).toBe(1);
    expect(cap.stderr).toContain('unknown flag: --bogus');
  });
});

// ─── End-to-end happy paths through the dispatcher ──────────────────────

describe('runCli — end-to-end smoke', () => {
  it('send + ls + read flow works through the dispatcher', async () => {
    setupIdentity('alice');
    setupIdentity('bob'); // need full folder so the ls/read on bob can resolve
    // send: stdin is the body; --from alice writes from alice to bob.
    const send = makeContext('hello bob', { ST_AGENT: 'alice' });
    const sendCode = await runCli(
      ['message', 'send', 'bob', '--from', 'alice', '--subject', 'hi'],
      send.ctx
    );
    expect(sendCode).toBe(0);
    const filename = send.stdout.trim();
    expect(filename).toMatch(/^[0-9]{13}-[0-9a-z]{6}\.md$/);

    // ls bob: header on stderr, filename on stdout.
    const lsCap = makeContext('', { ST_AGENT: 'alice' });
    const lsCode = await runCli(['message', 'ls', 'bob'], lsCap.ctx);
    expect(lsCode).toBe(0);
    expect(lsCap.stdout).toBe(`${filename}\n`);
    expect(lsCap.stderr).toContain('# 1 message in inbox');

    // read bob <filename>: header on stderr, body on stdout.
    const readCap = makeContext('', { ST_AGENT: 'alice' });
    const readCode = await runCli(
      ['message', 'read', 'bob', filename],
      readCap.ctx
    );
    expect(readCode).toBe(0);
    expect(readCap.stdout).toBe('hello bob\n');
    expect(readCap.stderr).toContain('# inbox/');
    expect(readCap.stderr).toContain('subject:     hi');
  });

  it('status get/set roundtrip', async () => {
    setupIdentity('alice');
    const setCap = makeContext('', { ST_AGENT: 'alice' });
    await runCli(['status', '--set', 'busy'], setCap.ctx);
    expect(setCap.stdout).toBe('status: busy\n');
    expect(readFileSync(join(stRoot, 'alice', 'status'), 'utf8')).toBe(
      'busy\n'
    );

    const getCap = makeContext('', { ST_AGENT: 'alice' });
    await runCli(['status'], getCap.ctx);
    expect(getCap.stdout).toBe('busy\n');
  });

  it('archive [<id>] <filename> moves the file', async () => {
    setupIdentity('bob');
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(
      join(stRoot, 'bob', 'inbox', f),
      `---\nfrom: alice\n---\nbody\n`
    );
    const cap = makeContext('', { ST_AGENT: 'bob' });
    const code = await runCli(['message', 'archive', f], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stderr).toContain('archived');
    expect(existsSync(join(stRoot, 'bob', 'inbox', f))).toBe(false);
    expect(existsSync(join(stRoot, 'bob', 'archive', f))).toBe(true);
  });
});
