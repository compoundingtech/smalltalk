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
let coordRoot: string;
let coordConfig: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-cli-test-'));
  coordRoot = join(scratch, 'coord');
  coordConfig = join(scratch, 'config');
  mkdirSync(coordRoot, { recursive: true });
  mkdirSync(coordConfig, { recursive: true });
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
    coordRoot,
    coordConfig,
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
  mkdirSync(join(coordRoot, id, 'inbox'), { recursive: true });
  mkdirSync(join(coordRoot, id, 'archive'), { recursive: true });
}

// ─── No args / help ─────────────────────────────────────────────────────

describe('runCli — no args / help', () => {
  it('no args → usage to stderr, exit 2', async () => {
    const cap = makeContext();
    const code = await runCli([], cap.ctx);
    expect(code).toBe(2);
    expect(cap.stdout).toBe('');
    expect(cap.stderr).toContain('usage: coord');
  });

  it('help → usage to stdout, exit 0', async () => {
    const cap = makeContext();
    const code = await runCli(['help'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stdout).toContain('usage: coord');
  });

  it('--help → usage to stdout, exit 0', async () => {
    const cap = makeContext();
    const code = await runCli(['--help'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stdout).toContain('usage: coord');
  });

  it('-h → usage to stdout, exit 0', async () => {
    const cap = makeContext();
    const code = await runCli(['-h'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stdout).toContain('usage: coord');
  });
});

// ─── Unknown subcommand ─────────────────────────────────────────────────

describe('runCli — unknown subcommand', () => {
  it('prints "unknown subcommand" + usage on stderr, exit 2', async () => {
    const cap = makeContext();
    const code = await runCli(['bogus'], cap.ctx);
    expect(code).toBe(2);
    expect(cap.stderr).toContain('unknown subcommand: bogus');
    expect(cap.stderr).toContain('usage: coord');
  });
});

// ─── Per-command --help ─────────────────────────────────────────────────

describe('runCli — per-command --help / -h prints command-specific usage', () => {
  // Brief-017: message verbs are nested under `coord message`; the
  // CLI surfaces them via `coord message <verb> --help` (or the `msg`
  // alias). Non-message verbs remain top-level.
  it.each([
    [['message', 'send'], 'usage: coord message send'],
    [['message', 'ls'], 'usage: coord message ls'],
    [['message', 'read'], 'usage: coord message read'],
    [['message', 'archive'], 'usage: coord message archive'],
    [['message', 'thread'], 'usage: coord message thread'],
    [['msg', 'send'], 'usage: coord message send'],
    [['watch'], 'usage: coord watch'],
    [['status'], 'usage: coord status'],
    [['sync'], 'usage: coord sync'],
  ] as const)('%j --help', async (cmd, prefix) => {
    const cap = makeContext();
    const code = await runCli([...cmd, '--help'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stderr).toContain(prefix);
  });

  it.each([
    [['message', 'send'], 'usage: coord message send'],
    [['message', 'ls'], 'usage: coord message ls'],
  ] as const)('%j -h', async (cmd, prefix) => {
    const cap = makeContext();
    const code = await runCli([...cmd, '-h'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stderr).toContain(prefix);
  });

  it('top-level `coord send` (pre-brief-017 flat form) errors with a pointer', async () => {
    const cap = makeContext();
    const code = await runCli(['send', 'bob'], cap.ctx);
    expect(code).toBe(2);
    expect(cap.stderr).toContain('Did you mean `coord message send`?');
  });

  it('`coord message --help` prints the message-group banner', async () => {
    const cap = makeContext();
    const code = await runCli(['message', '--help'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stdout).toContain('coord message <verb>');
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
    writeFileSync(join(coordRoot, 'bob', 'inbox', f), 'same');
    writeFileSync(join(coordRoot, 'bob', 'archive', f), 'same');
    const cap = makeContext();
    const code = await runCli(['message', 'ls', 'bob'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stdout).toContain(f);
    // Zombie still on disk after ls.
    expect(existsSync(join(coordRoot, 'bob', 'inbox', f))).toBe(true);
  });

  it('`read` lazy-sweeps the zombie inbox copy (byte-identical twin)', async () => {
    setupIdentity('bob');
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(join(coordRoot, 'bob', 'inbox', f), 'same');
    writeFileSync(join(coordRoot, 'bob', 'archive', f), 'same');
    const cap = makeContext();
    const code = await runCli(['message', 'read', 'bob', f], cap.ctx);
    expect(code).toBe(0);
    // Lazy-read sweep cleaned up the inbox copy.
    expect(existsSync(join(coordRoot, 'bob', 'inbox', f))).toBe(false);
    expect(existsSync(join(coordRoot, 'bob', 'archive', f))).toBe(true);
  });
});

// ─── Errors are surfaced as `coord: <msg>` on stderr, exit 1 ────────────

describe('runCli — error formatting', () => {
  it('command throw becomes "coord: <message>" on stderr, exit 1', async () => {
    // No identity context = identity-required error from cmdLs.
    const cap = makeContext();
    const code = await runCli(['message', 'ls'], cap.ctx);
    expect(code).toBe(1);
    expect(cap.stderr).toMatch(/^coord: identity required/);
  });

  it('unknown flag → exit 1 with "unknown flag" message', async () => {
    setupIdentity('bob');
    const cap = makeContext('', { COORD_IDENTITY: 'bob' });
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
    const send = makeContext('hello bob', { COORD_IDENTITY: 'alice' });
    const sendCode = await runCli(
      ['message', 'send', 'bob', '--from', 'alice', '--subject', 'hi'],
      send.ctx
    );
    expect(sendCode).toBe(0);
    const filename = send.stdout.trim();
    expect(filename).toMatch(/^[0-9]{13}-[0-9a-z]{6}\.md$/);

    // ls bob: header on stderr, filename on stdout.
    const lsCap = makeContext('', { COORD_IDENTITY: 'alice' });
    const lsCode = await runCli(['message', 'ls', 'bob'], lsCap.ctx);
    expect(lsCode).toBe(0);
    expect(lsCap.stdout).toBe(`${filename}\n`);
    expect(lsCap.stderr).toContain('# 1 message in inbox');

    // read bob <filename>: header on stderr, body on stdout.
    const readCap = makeContext('', { COORD_IDENTITY: 'alice' });
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
    const setCap = makeContext('', { COORD_IDENTITY: 'alice' });
    await runCli(['status', '--set', 'busy'], setCap.ctx);
    expect(setCap.stdout).toBe('status: busy\n');
    expect(readFileSync(join(coordRoot, 'alice', 'status'), 'utf8')).toBe(
      'busy\n'
    );

    const getCap = makeContext('', { COORD_IDENTITY: 'alice' });
    await runCli(['status'], getCap.ctx);
    expect(getCap.stdout).toBe('busy\n');
  });

  it('archive [<id>] <filename> moves the file', async () => {
    setupIdentity('bob');
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(
      join(coordRoot, 'bob', 'inbox', f),
      `---\nfrom: alice\n---\nbody\n`
    );
    const cap = makeContext('', { COORD_IDENTITY: 'bob' });
    const code = await runCli(['message', 'archive', f], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stderr).toContain('archived');
    expect(existsSync(join(coordRoot, 'bob', 'inbox', f))).toBe(false);
    expect(existsSync(join(coordRoot, 'bob', 'archive', f))).toBe(true);
  });
});
