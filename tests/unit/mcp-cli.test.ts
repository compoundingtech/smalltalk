// tests/unit/mcp-cli.test.ts — env contract for `coord mcp` CLI.
//
// The CLI's env handling diverged from every other coord verb until
// brief-021 unified it: COORD_IDENTITY is required (no default),
// COORD_ROOT defaults to ~/.local/state/coord via coordRootFrom().
// These tests pin both halves of that contract.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { CliContext } from '../../src/cli-context.ts';
import { cmdMcpCli } from '../../src/commands/mcp.ts';

// Stub the heavy MCP module so cmdMcpCli's dynamic import lands on
// our spy instead of booting a real stdio server. Vitest hoists
// vi.mock and applies it to dynamic imports through the shared
// module registry.
const createMcpServerSpy = vi.fn();
vi.mock('../../src/mcp/index.ts', () => ({
  createMcpServer: (opts: unknown) => {
    createMcpServerSpy(opts);
    return {
      mcp: {},
      coord: {},
      async run() {
        /* no-op */
      },
      async close() {
        /* no-op */
      },
      async runWith() {
        /* no-op */
      },
      async startChannelWatcher() {
        /* no-op */
      },
    };
  },
}));

function makeCtx(env: NodeJS.ProcessEnv): CliContext {
  return {
    env,
    coordRoot: '/unused-by-mcp',
    coordConfig: '/unused-by-mcp',
    stdout: () => {},
    stderr: () => {},
    readStdin: async () => Buffer.alloc(0),
  };
}

beforeEach(() => {
  createMcpServerSpy.mockClear();
});

afterEach(() => {
  vi.unstubAllEnvs();
});

describe('cmdMcpCli — env contract', () => {
  it('throws when COORD_IDENTITY is unset (no default)', async () => {
    await expect(cmdMcpCli([], makeCtx({}))).rejects.toThrow(/COORD_IDENTITY/);
  });

  it('throws when COORD_IDENTITY is empty string', async () => {
    await expect(
      cmdMcpCli([], makeCtx({ COORD_IDENTITY: '' }))
    ).rejects.toThrow(/COORD_IDENTITY/);
  });

  it('boots with only COORD_IDENTITY set; COORD_ROOT defaults', async () => {
    await cmdMcpCli([], makeCtx({ COORD_IDENTITY: 'alice' }));
    expect(createMcpServerSpy).toHaveBeenCalledTimes(1);
    const opts = createMcpServerSpy.mock.calls[0]?.[0] as {
      root: string;
      identity: string;
      channel: boolean;
    };
    expect(opts.identity).toBe('alice');
    expect(opts.channel).toBe(false);
    // Default root is ~/.local/state/smalltalk (phase 0 rename) —
    // exact path is host-dependent, but the suffix is stable.
    expect(opts.root.endsWith('.local/state/smalltalk')).toBe(true);
  });

  it('explicit COORD_ROOT wins over the default', async () => {
    await cmdMcpCli(
      [],
      makeCtx({ COORD_IDENTITY: 'alice', COORD_ROOT: '/tmp/some-root' })
    );
    expect(createMcpServerSpy).toHaveBeenCalledTimes(1);
    const opts = createMcpServerSpy.mock.calls[0]?.[0] as { root: string };
    expect(opts.root).toBe('/tmp/some-root');
  });

  it('--channel flag flips channel mode on', async () => {
    await cmdMcpCli(['--channel'], makeCtx({ COORD_IDENTITY: 'alice' }));
    const opts = createMcpServerSpy.mock.calls[0]?.[0] as { channel: boolean };
    expect(opts.channel).toBe(true);
  });

  it('--help short-circuits before any env check', async () => {
    let helpText = '';
    const ctx = makeCtx({});
    ctx.stderr = (s) => {
      helpText += s;
    };
    const rc = await cmdMcpCli(['--help'], ctx);
    expect(rc).toBe(0);
    expect(helpText).toContain('coord mcp');
    expect(helpText).toContain('COORD_IDENTITY');
    expect(createMcpServerSpy).not.toHaveBeenCalled();
  });
});
