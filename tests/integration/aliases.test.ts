// tests/integration/aliases.test.ts — brief-005-phase0 acceptance items
// 1 (binary aliases) and 6 (plugin proxy). These need to shell out to
// real bash so they live in integration; the other four acceptance
// items are covered as unit tests in tests/unit/aliases.test.ts.

import { spawnSync } from 'node:child_process';
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { REPO_ROOT } from './helpers.ts';

const BIN_COORD = join(REPO_ROOT, 'bin', 'coord');
const BIN_ST = join(REPO_ROOT, 'bin', 'st');
const BIN_SMALLTALK = join(REPO_ROOT, 'bin', 'smalltalk');

let scratch: string;
beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-alias-it-'));
});
afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

function runBin(
  bin: string,
  args: readonly string[],
  env: NodeJS.ProcessEnv = {}
): { stdout: string; stderr: string; exitCode: number } {
  const r = spawnSync(bin, args, {
    encoding: 'utf8',
    env: {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
      ...env,
    },
    timeout: 30_000,
  });
  return {
    stdout: typeof r.stdout === 'string' ? r.stdout : '',
    stderr: typeof r.stderr === 'string' ? r.stderr : '',
    exitCode: r.status ?? -1,
  };
}

// ─── Item 1: binary aliases ───────────────────────────────────────────────

describe('binary aliases (Item 1)', () => {
  it('bin/coord help exits 0 with usage', () => {
    const r = runBin(BIN_COORD, ['help']);
    expect(r.exitCode).toBe(0);
    expect(r.stdout).toContain('usage: coord');
  });

  it('bin/st help exits 0 with usage', () => {
    const r = runBin(BIN_ST, ['help']);
    expect(r.exitCode).toBe(0);
    expect(r.stdout).toContain('usage: coord');
  });

  it('bin/smalltalk help exits 0 with usage', () => {
    const r = runBin(BIN_SMALLTALK, ['help']);
    expect(r.exitCode).toBe(0);
    expect(r.stdout).toContain('usage: coord');
  });

  it('all three binaries produce IDENTICAL output for `help`', () => {
    const c = runBin(BIN_COORD, ['help']);
    const s = runBin(BIN_ST, ['help']);
    const t = runBin(BIN_SMALLTALK, ['help']);
    expect(s.stdout).toBe(c.stdout);
    expect(t.stdout).toBe(c.stdout);
    expect(s.exitCode).toBe(c.exitCode);
    expect(t.exitCode).toBe(c.exitCode);
  });

  it('all three propagate _ST_INVOKED_AS when invoked directly (status round-trip)', () => {
    // The cleanest end-to-end probe of the shim's invoked-as capture
    // is a verb that depends on env / identity working. `coord status
    // <id>` with no value just reads the file — and it works under all
    // three names with a freshly-created scratch root.
    const root = join(scratch, 'root');
    const env = { ST_ROOT: root, ST_IDENTITY: 'tester' };
    runBin(BIN_COORD, ['status', '--set', 'available'], env);
    const r1 = runBin(BIN_COORD, ['status'], env);
    const r2 = runBin(BIN_ST, ['status'], env);
    const r3 = runBin(BIN_SMALLTALK, ['status'], env);
    expect(r1.stdout.trim()).toBe('available');
    expect(r2.stdout).toBe(r1.stdout);
    expect(r3.stdout).toBe(r1.stdout);
  });
});

// ─── Item 6: plugin proxy ─────────────────────────────────────────────────

describe('plugin proxy (Item 6)', () => {
  function writePlugin(
    pluginName: string,
    contents: string
  ): string {
    const path = join(scratch, pluginName);
    writeFileSync(
      path,
      `#!/usr/bin/env bash\n${contents}\n`
    );
    chmodSync(path, 0o755);
    return path;
  }

  it('`coord <cmd>` execs `st-<cmd>` from PATH with the rest of argv', () => {
    writePlugin(
      'st-hello',
      'echo "hello-args: $*"\nexit 0'
    );
    const r = runBin(
      BIN_COORD,
      ['hello', 'one', 'two', '--flag'],
      { PATH: `${scratch}:${process.env.PATH ?? ''}` }
    );
    expect(r.exitCode).toBe(0);
    expect(r.stdout.trim()).toBe('hello-args: one two --flag');
  });

  it('also matches smalltalk-<cmd> when st-<cmd> is absent', () => {
    writePlugin(
      'smalltalk-greet',
      'echo "via smalltalk prefix"\nexit 0'
    );
    const r = runBin(BIN_COORD, ['greet'], {
      PATH: `${scratch}:${process.env.PATH ?? ''}`,
    });
    expect(r.exitCode).toBe(0);
    expect(r.stdout.trim()).toBe('via smalltalk prefix');
  });

  it('also matches coord-<cmd> as the legacy fallback prefix', () => {
    writePlugin(
      'coord-legacy',
      'echo "via coord prefix"\nexit 0'
    );
    const r = runBin(BIN_COORD, ['legacy'], {
      PATH: `${scratch}:${process.env.PATH ?? ''}`,
    });
    expect(r.exitCode).toBe(0);
    expect(r.stdout.trim()).toBe('via coord prefix');
  });

  it('precedence: st-<cmd> wins over coord-<cmd> when both exist', () => {
    writePlugin('st-pick', 'echo "from st"\nexit 0');
    writePlugin('coord-pick', 'echo "from coord"\nexit 1');
    const r = runBin(BIN_COORD, ['pick'], {
      PATH: `${scratch}:${process.env.PATH ?? ''}`,
    });
    expect(r.exitCode).toBe(0);
    expect(r.stdout.trim()).toBe('from st');
  });

  it('built-in commands ALWAYS win over plugins of the same name', () => {
    // `members` is a built-in. A plugin named st-members must NOT
    // shadow it; coord's own list-members runs instead.
    writePlugin(
      'st-members',
      'echo "PLUGIN-SHOULD-NOT-RUN"\nexit 1'
    );
    const r = runBin(BIN_COORD, ['members'], {
      ST_ROOT: scratch, // empty root → no members; clean exit
      PATH: `${scratch}:${process.env.PATH ?? ''}`,
    });
    expect(r.exitCode).toBe(0);
    expect(r.stdout).not.toContain('PLUGIN-SHOULD-NOT-RUN');
  });

  it('plugin exit code propagates to the parent', () => {
    writePlugin('st-fail', 'exit 7');
    const r = runBin(BIN_COORD, ['fail'], {
      PATH: `${scratch}:${process.env.PATH ?? ''}`,
    });
    expect(r.exitCode).toBe(7);
  });

  it('unknown command with no matching plugin → exit 2 with usage', () => {
    const r = runBin(
      BIN_COORD,
      ['this-command-definitely-does-not-exist'],
      // Keep parent PATH so the shim can still find `node` to exec —
      // but the scratch dir on its own contains no plugins matching
      // our nonsense verb.
      { PATH: `${scratch}:${process.env.PATH ?? ''}` }
    );
    expect(r.exitCode).toBe(2);
    expect(r.stderr).toContain('unknown subcommand');
  });
});
