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

// ─── Binary aliases (post-coord-cutover: st + smalltalk only) ────────────

describe('binary aliases', () => {
  it('bin/st help exits 0 with usage', () => {
    const r = runBin(BIN_ST, ['help']);
    expect(r.exitCode).toBe(0);
    expect(r.stdout).toContain('usage: st');
  });

  it('bin/smalltalk help exits 0 with usage', () => {
    const r = runBin(BIN_SMALLTALK, ['help']);
    expect(r.exitCode).toBe(0);
    expect(r.stdout).toContain('usage: smalltalk');
  });

  it('both binaries dispatch identically (exit + subcommand list) for `help`', () => {
    const s = runBin(BIN_ST, ['help']);
    const t = runBin(BIN_SMALLTALK, ['help']);
    expect(t.exitCode).toBe(s.exitCode);
    // Normalize the invoked-name interpolation and compare the rest.
    const strip = (str: string): string =>
      str.replace(/\b(smalltalk|st)\b/g, 'NAME');
    expect(strip(t.stdout)).toBe(strip(s.stdout));
  });

  it('bin/coord is REMOVED (regression guard for the coord-cutover)', async () => {
    const { existsSync } = await import('node:fs');
    expect(existsSync(BIN_COORD)).toBe(false);
  });
});

// ─── Plugin proxy (git-style PATH dispatch) ──────────────────────────────

describe('plugin proxy', () => {
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

  it('`st <cmd>` execs `st-<cmd>` from PATH with the rest of argv', () => {
    writePlugin('st-hello', 'echo "hello-args: $*"\nexit 0');
    const r = runBin(
      BIN_ST,
      ['hello', 'one', 'two', '--flag'],
      { PATH: `${scratch}:${process.env.PATH ?? ''}` }
    );
    expect(r.exitCode).toBe(0);
    expect(r.stdout.trim()).toBe('hello-args: one two --flag');
  });

  it('also matches smalltalk-<cmd> when st-<cmd> is absent', () => {
    writePlugin('smalltalk-greet', 'echo "via smalltalk prefix"\nexit 0');
    const r = runBin(BIN_ST, ['greet'], {
      PATH: `${scratch}:${process.env.PATH ?? ''}`,
    });
    expect(r.exitCode).toBe(0);
    expect(r.stdout.trim()).toBe('via smalltalk prefix');
  });

  it('coord-<cmd> is NO LONGER scanned (regression guard for the coord-cutover)', () => {
    writePlugin('coord-legacy', 'echo "should not run"\nexit 0');
    const r = runBin(BIN_ST, ['legacy'], {
      PATH: `${scratch}:${process.env.PATH ?? ''}`,
    });
    // Unknown verb — coord- prefix is retired, so no plugin matches.
    expect(r.exitCode).toBe(2);
    expect(r.stderr).toContain('unknown subcommand');
  });

  it('precedence: st-<cmd> wins over smalltalk-<cmd> when both exist', () => {
    writePlugin('st-pick', 'echo "from st"\nexit 0');
    writePlugin('smalltalk-pick', 'echo "from smalltalk"\nexit 1');
    const r = runBin(BIN_ST, ['pick'], {
      PATH: `${scratch}:${process.env.PATH ?? ''}`,
    });
    expect(r.exitCode).toBe(0);
    expect(r.stdout.trim()).toBe('from st');
  });

  it('built-in commands ALWAYS win over plugins of the same name', () => {
    // `agents` is a built-in. A plugin named st-agents must NOT
    // shadow it; the built-in runs instead.
    writePlugin('st-agents', 'echo "PLUGIN-SHOULD-NOT-RUN"\nexit 1');
    const r = runBin(BIN_ST, ['agents'], {
      ST_ROOT: scratch, // empty root → no agents; clean exit
      PATH: `${scratch}:${process.env.PATH ?? ''}`,
    });
    expect(r.exitCode).toBe(0);
    expect(r.stdout).not.toContain('PLUGIN-SHOULD-NOT-RUN');
  });

  it('plugin exit code propagates to the parent', () => {
    writePlugin('st-fail', 'exit 7');
    const r = runBin(BIN_ST, ['fail'], {
      PATH: `${scratch}:${process.env.PATH ?? ''}`,
    });
    expect(r.exitCode).toBe(7);
  });

  it('unknown command with no matching plugin → exit 2 with usage', () => {
    const r = runBin(
      BIN_ST,
      ['this-command-definitely-does-not-exist'],
      { PATH: `${scratch}:${process.env.PATH ?? ''}` }
    );
    expect(r.exitCode).toBe(2);
    expect(r.stderr).toContain('unknown subcommand');
  });
});
