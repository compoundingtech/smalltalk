// tests/unit/completions.test.ts — coverage for `st completions <shell>`.
//
// Exercises the dispatcher path (runCli) and the generated-script shape for
// each supported shell, plus the enum value sets that the spec derives from
// the canonical SETTABLE_STATES / PRIORITIES constants.

import { describe, expect, it } from 'vitest';

import { runCli, type CliContext } from '../../src/cli.ts';
import { SETTABLE_STATES } from '../../src/common.ts';
import { PRIORITIES } from '../../src/types.ts';

interface Capture {
  stdout: string;
  stderr: string;
  ctx: CliContext;
}

function makeContext(): Capture {
  const cap: Capture = {
    stdout: '',
    stderr: '',
    ctx: undefined as unknown as CliContext,
  };
  cap.ctx = {
    env: {},
    stRoot: '/tmp/does-not-matter',
    stConfig: '/tmp/does-not-matter',
    stdout: (s) => {
      cap.stdout += s;
    },
    stderr: (s) => {
      cap.stderr += s;
    },
    readStdin: async () => Buffer.alloc(0),
  };
  return cap;
}

describe('coord completions — dispatch', () => {
  it('fish → non-empty script with `complete -c st`, exit 0', async () => {
    const cap = makeContext();
    const code = await runCli(['completions', 'fish'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stdout).toContain('complete -c st');
    expect(cap.stdout.length).toBeGreaterThan(100);
    expect(cap.stderr).toBe('');
  });

  it.each(['fish', 'bash', 'zsh'] as const)(
    '%s → non-empty script, exit 0',
    async (shell) => {
      const cap = makeContext();
      const code = await runCli(['completions', shell], cap.ctx);
      expect(code).toBe(0);
      expect(cap.stdout.length).toBeGreaterThan(100);
    }
  );

  it('missing shell → usage on stderr, exit 2', async () => {
    const cap = makeContext();
    const code = await runCli(['completions'], cap.ctx);
    expect(code).toBe(2);
    expect(cap.stdout).toBe('');
    expect(cap.stderr).toContain('usage: st completions');
  });

  it('unknown shell → usage on stderr, exit 2', async () => {
    const cap = makeContext();
    const code = await runCli(['completions', 'powershell'], cap.ctx);
    expect(code).toBe(2);
    expect(cap.stderr).toContain('unknown shell: powershell');
    expect(cap.stderr).toContain('usage: st completions');
  });

  it('--help → usage on stdout, exit 0', async () => {
    const cap = makeContext();
    const code = await runCli(['completions', '--help'], cap.ctx);
    expect(code).toBe(0);
    expect(cap.stdout).toContain('usage: st completions');
  });

  it('top-level usage advertises the completions subcommand', async () => {
    const cap = makeContext();
    await runCli(['--help'], cap.ctx);
    expect(cap.stdout).toContain('completions <shell>');
  });
});

describe('coord completions fish — surface', () => {
  async function fish(): Promise<string> {
    const cap = makeContext();
    await runCli(['completions', 'fish'], cap.ctx);
    return cap.stdout;
  }

  it('lists every top-level subcommand (incl. the msg alias)', async () => {
    const script = await fish();
    for (const sub of [
      'message',
      'msg',
      'watch',
      'status',
      'members',
      'overview',
      'resource',
      'sync',
      'mcp',
      'init',
      'ding',
      'completions',
    ]) {
      expect(script).toContain(`-a ${sub} `);
    }
  });

  it('binds the settable-state enum to `status --set`', async () => {
    const script = await fish();
    expect(script).toContain(
      `-l set -x -a '${SETTABLE_STATES.join(' ')}'`
    );
  });

  it('offers priorities for `--priority`', async () => {
    const script = await fish();
    expect(script).toContain(`-l priority -x -a '${PRIORITIES.join(' ')}'`);
  });
});
