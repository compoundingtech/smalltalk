// tests/unit/mcp-cli.test.ts — env contract for `st mcp` CLI.
//
// The CLI's env handling diverged from every other smalltalk verb until
// brief-021 unified it: ST_AGENT (or legacy ST_IDENTITY/ST_AGENT)
// resolves identity; ST_ROOT/ST_ROOT defaults to
// ~/.local/state/smalltalk via stRootFrom(). When no env identity
// is set, the server falls back to a throwaway `anon-<rand6>` identity
// (Nathan's call, per the Codex-host gap) rather than hard-exiting.
// These tests pin all three contracts.

import { mkdirSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
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
      smalltalk: {},
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
    stRoot: '/unused-by-mcp',
    stConfig: '/unused-by-mcp',
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
  it('boots with only ST_AGENT set; ST_ROOT defaults', async () => {
    await cmdMcpCli([], makeCtx({ ST_AGENT: 'alice' }));
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

  it('explicit ST_ROOT wins over the default', async () => {
    await cmdMcpCli(
      [],
      makeCtx({ ST_AGENT: 'alice', ST_ROOT: '/tmp/some-root' })
    );
    expect(createMcpServerSpy).toHaveBeenCalledTimes(1);
    const opts = createMcpServerSpy.mock.calls[0]?.[0] as { root: string };
    expect(opts.root).toBe('/tmp/some-root');
  });

  it('--channel flag flips channel mode on', async () => {
    await cmdMcpCli(['--channel'], makeCtx({ ST_AGENT: 'alice' }));
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
    expect(helpText).toContain('st mcp');
    expect(helpText).toContain('ST_AGENT');
    expect(createMcpServerSpy).not.toHaveBeenCalled();
  });
});

// ─── Anon fallback (Nathan's brief; Codex-host gap) ────────────────────

describe('cmdMcpCli — anon-identity fallback', () => {
  let scratchRoot: string;

  beforeEach(() => {
    scratchRoot = mkdtempSync(join(tmpdir(), 'st-mcp-anon-'));
  });

  afterEach(() => {
    rmSync(scratchRoot, { recursive: true, force: true });
  });

  it('no ST_AGENT / ST_IDENTITY / ST_AGENT set → boots with an anon-<rand6> identity', async () => {
    await cmdMcpCli([], makeCtx({ ST_ROOT: scratchRoot }));
    expect(createMcpServerSpy).toHaveBeenCalledTimes(1);
    const opts = createMcpServerSpy.mock.calls[0]?.[0] as { identity: string };
    expect(opts.identity).toMatch(/^anon-[0-9a-z]{6}$/);
  });

  it('writes a one-line stderr warning naming the throwaway identity + pointing at ST_AGENT', async () => {
    let stderrCapture = '';
    const ctx = makeCtx({ ST_ROOT: scratchRoot });
    ctx.stderr = (s) => {
      stderrCapture += s;
    };
    await cmdMcpCli([], ctx);
    expect(stderrCapture).toMatch(/no ST_AGENT set/);
    expect(stderrCapture).toMatch(/anon-[0-9a-z]{6}/);
    expect(stderrCapture).toMatch(/set ST_AGENT to persist/);
    // Exactly one warning line.
    expect(stderrCapture.match(/\n/g)?.length).toBe(1);
  });

  it('creates the anon agent\'s inbox + archive folders on disk', async () => {
    let stderrCapture = '';
    const ctx = makeCtx({ ST_ROOT: scratchRoot });
    ctx.stderr = (s) => {
      stderrCapture += s;
    };
    await cmdMcpCli([], ctx);
    const m = /anon-[0-9a-z]{6}/.exec(stderrCapture);
    expect(m).toBeTruthy();
    const id = m![0];
    const { existsSync } = await import('node:fs');
    expect(existsSync(join(scratchRoot, id, 'inbox'))).toBe(true);
    expect(existsSync(join(scratchRoot, id, 'archive'))).toBe(true);
  });

  it('each invocation generates a fresh throwaway identity (no leakage across spawns)', async () => {
    const ids: string[] = [];
    for (let i = 0; i < 3; i++) {
      createMcpServerSpy.mockClear();
      await cmdMcpCli([], makeCtx({ ST_ROOT: scratchRoot }));
      const opts = createMcpServerSpy.mock.calls[0]?.[0] as {
        identity: string;
      };
      ids.push(opts.identity);
    }
    expect(new Set(ids).size).toBe(3);
  });

  it('explicit ST_AGENT wins — no fallback, no warning', async () => {
    let stderrCapture = '';
    const ctx = makeCtx({ ST_ROOT: scratchRoot, ST_AGENT: 'alice' });
    ctx.stderr = (s) => {
      stderrCapture += s;
    };
    await cmdMcpCli([], ctx);
    const opts = createMcpServerSpy.mock.calls[0]?.[0] as { identity: string };
    expect(opts.identity).toBe('alice');
    expect(stderrCapture).not.toMatch(/anon-/);
    expect(stderrCapture).not.toMatch(/no ST_AGENT set/);
  });

  it('legacy ST_AGENT still wins over fallback (with the existing deprecation notice)', async () => {
    let stderrCapture = '';
    const ctx = makeCtx({
      ST_ROOT: scratchRoot,
      ST_AGENT: 'legacy-bob',
    });
    ctx.stderr = (s) => {
      stderrCapture += s;
    };
    await cmdMcpCli([], ctx);
    const opts = createMcpServerSpy.mock.calls[0]?.[0] as { identity: string };
    expect(opts.identity).toBe('legacy-bob');
    expect(stderrCapture).not.toMatch(/anon-/);
  });
});
