// tests/unit/aliases.test.ts — brief-005-phase0 acceptance items.
//
// Covers four of the six Phase 0 acceptance criteria as unit tests:
//   2. MCP server announces under the right name (coord vs st).
//   3. MCP tools are registered under `st_*` only (coord_* alias removed).
//   4. ST_* env vars are preferred over COORD_* (with one-time warning).
//   5. State-dir resolution prefers `smalltalk/` over `coord/` with
//      `smalltalk/` as the brand-new-install default.
//
// Items 1 (binary aliases) and 6 (plugin proxy) shell out, so they're
// integration tests — see tests/integration/aliases.test.ts.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import {
  mkdirSync,
  mkdtempSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  afterEach,
  beforeEach,
  describe,
  expect,
  it,
  vi,
} from 'vitest';

import {
  _resetLegacyEnvFallbackWarnings,
  canonicalServerName,
  stRootFrom,
  envIdentityFrom,
  invokedAsFrom,
  resolveIdentity,
} from '../../src/common.ts';
import { buildServerInfo } from '../../src/mcp/capabilities.ts';
import { createMcpServer } from '../../src/mcp/index.ts';
import { asIdentity } from '../../src/types.ts';

let scratch: string;
beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-aliases-test-'));
  _resetLegacyEnvFallbackWarnings();
});
afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
  vi.restoreAllMocks();
});

// ─── Item 4: env-var dual-honor ───────────────────────────────────────────

describe('env-var resolution (ST_* only; no COORD_* fallback)', () => {
  it('stRootFrom: honors $ST_ROOT when set', () => {
    const env = { ST_ROOT: '/tmp/isolated' } as NodeJS.ProcessEnv;
    expect(stRootFrom(env)).toBe('/tmp/isolated');
  });

  it('stRootFrom: $COORD_ROOT is NOT honored (post-cutover)', () => {
    // Regression guard for the coord-cutover: setting $COORD_ROOT
    // must NOT be picked up. stRootFrom falls back to the default
    // state root instead.
    const env = { COORD_ROOT: '/tmp/legacy' } as NodeJS.ProcessEnv;
    expect(stRootFrom(env)).not.toBe('/tmp/legacy');
    expect(stRootFrom(env)).toMatch(/\.local\/state\/smalltalk$/);
  });

  it('envIdentityFrom: ST_AGENT wins over ST_IDENTITY when both are set', () => {
    const env = {
      ST_AGENT: 'newname',
      ST_IDENTITY: 'oldname',
    } as NodeJS.ProcessEnv;
    expect(envIdentityFrom(env)).toBe('newname');
  });

  it('envIdentityFrom: ST_IDENTITY used as fallback (warns once)', () => {
    const env = { ST_IDENTITY: 'oldname' } as NodeJS.ProcessEnv;
    expect(envIdentityFrom(env)).toBe('oldname');
  });

  it('envIdentityFrom: $COORD_IDENTITY is NOT honored (post-cutover)', () => {
    // Regression guard: coord-era env alias no longer resolves.
    const env = { COORD_IDENTITY: 'legacy' } as NodeJS.ProcessEnv;
    expect(envIdentityFrom(env)).toBeUndefined();
  });

  it('envIdentityFrom: neither set → undefined (caller throws own error)', () => {
    expect(envIdentityFrom({} as NodeJS.ProcessEnv)).toBeUndefined();
  });

  it('envIdentityFrom: warns once when honoring the legacy ST_IDENTITY', () => {
    const spy = vi.spyOn(process.stderr, 'write').mockReturnValue(true);
    const env = { ST_IDENTITY: 'legacyname' } as NodeJS.ProcessEnv;
    envIdentityFrom(env);
    envIdentityFrom(env);
    const warnCalls = spy.mock.calls.filter((c) =>
      String(c[0]).includes('ST_IDENTITY')
    );
    expect(warnCalls).toHaveLength(1);
    expect(String(warnCalls[0]![0])).toContain('migrate to ST_AGENT');
    spy.mockRestore();
  });

  it('resolveIdentity: ST_AGENT preferred over ST_IDENTITY', () => {
    const root = join(scratch, 'state');
    mkdirSync(root, { recursive: true });
    const env = {
      ST_AGENT: 'primary',
      ST_IDENTITY: 'secondary',
    } as NodeJS.ProcessEnv;
    expect(resolveIdentity({ env, stRoot: root })).toBe('primary');
  });

  it('resolveIdentity: $COORD_IDENTITY is NOT honored (post-cutover)', () => {
    const root = join(scratch, 'state');
    mkdirSync(root, { recursive: true });
    const env = { COORD_IDENTITY: 'legacy' } as NodeJS.ProcessEnv;
    expect(() => resolveIdentity({ env, stRoot: root })).toThrow(
      /agent required/
    );
  });
});

// ─── Item 5: state-dir resolution (post-cutover: smalltalk/ only) ────────

describe('state-dir resolution (~/.local/state/smalltalk)', () => {
  function withFakeHome(setup: (home: string) => void): string {
    const home = join(scratch, 'home');
    mkdirSync(home, { recursive: true });
    setup(home);
    return stRootFrom({ HOME: home } as NodeJS.ProcessEnv);
  }

  it('always resolves to ~/.local/state/smalltalk (no coord fallback)', () => {
    // Post-cutover: even when `~/.local/state/coord` exists on the
    // machine, it's ignored. `smalltalk/` is the sole default state
    // root.
    const r = withFakeHome((home) => {
      mkdirSync(join(home, '.local/state/coord'), { recursive: true });
    });
    expect(r).toMatch(/\.local\/state\/smalltalk$/);
  });

  it('NEITHER exists → default to smalltalk/ for fresh install', () => {
    const r = withFakeHome(() => {});
    expect(r).toMatch(/\.local\/state\/smalltalk$/);
  });

  it('ST_ROOT bypasses state-dir resolution entirely', () => {
    const r = withFakeHome((home) => {
      mkdirSync(join(home, '.local/state/coord'), { recursive: true });
    });
    void r; // setup
    expect(
      stRootFrom({ ST_ROOT: '/explicit', HOME: scratch } as NodeJS.ProcessEnv)
    ).toBe('/explicit');
  });
});

// ─── invokedAs + canonical-server-name helpers ────────────────────────────

describe('invokedAsFrom + canonicalServerName (post-coord-cutover)', () => {
  it('reads _ST_INVOKED_AS and accepts st + smalltalk', () => {
    expect(invokedAsFrom({ _ST_INVOKED_AS: 'st' } as NodeJS.ProcessEnv)).toBe(
      'st'
    );
    expect(
      invokedAsFrom({ _ST_INVOKED_AS: 'smalltalk' } as NodeJS.ProcessEnv)
    ).toBe('smalltalk');
  });

  it('defaults to `st` when env var is absent or unknown', () => {
    expect(invokedAsFrom({} as NodeJS.ProcessEnv)).toBe('st');
    expect(invokedAsFrom({ _ST_INVOKED_AS: 'nope' } as NodeJS.ProcessEnv)).toBe(
      'st'
    );
  });

  it('canonicalServerName: st + smalltalk both → st', () => {
    expect(canonicalServerName('st')).toBe('st');
    expect(canonicalServerName('smalltalk')).toBe('st');
  });
});

// ─── MCP server name (post-coord-cutover: always `st`) ───────────────────

describe('MCP server name (post-coord-cutover)', () => {
  it('buildServerInfo returns the name passed in', () => {
    expect(buildServerInfo('st').name).toBe('st');
  });

  it('server announces "st" by default (no coord back-compat)', async () => {
    const root = join(scratch, 'state');
    mkdirSync(join(root, 'tester', 'inbox'), { recursive: true });
    mkdirSync(join(root, 'tester', 'archive'), { recursive: true });
    const handle = createMcpServer({
      root,
      identity: asIdentity('tester'),
    });
    const [a, b] = InMemoryTransport.createLinkedPair();
    const client = new Client({ name: 'aliases-test', version: '0' });
    await Promise.all([client.connect(a), handle.mcp.connect(b)]);
    try {
      expect(client.getServerVersion()?.name).toBe('st');
    } finally {
      await client.close();
      await handle.close();
    }
  });
});

// ─── Item 3 (post-cutover): tool name registry is `st_*` only ─────────────
//
// The historical dual-register (coord_* + st_* under the same
// handlers) has been removed — one canonical name each. This block
// locks in the st-only surface.

describe('MCP tool name registry (post-coord-cutover)', () => {
  async function connect(): Promise<{
    client: Client;
    close(): Promise<void>;
  }> {
    const root = join(scratch, 'state');
    mkdirSync(join(root, 'tester', 'inbox'), { recursive: true });
    mkdirSync(join(root, 'tester', 'archive'), { recursive: true });
    const handle = createMcpServer({
      root,
      identity: asIdentity('tester'),
    });
    const [a, b] = InMemoryTransport.createLinkedPair();
    const client = new Client({ name: 'aliases-test', version: '0' });
    await Promise.all([client.connect(a), handle.mcp.connect(b)]);
    return {
      client,
      close: async () => {
        await client.close();
        await handle.close();
      },
    };
  }

  it('listTools includes ONLY the `st_*` registrations — no coord_* prefix', async () => {
    const { client, close } = await connect();
    try {
      const r = await client.listTools();
      const names = new Set(r.tools.map((t) => t.name));
      // Every canonical `st_*` base name is present.
      for (const base of [
        'msg_send',
        'msg_ls',
        'msg_read',
        'msg_archive',
        'msg_thread',
        'agents',
      ]) {
        expect(names.has(`st_${base}`)).toBe(true);
        // Regression guard: the historic `coord_*` alias is GONE.
        expect(names.has(`coord_${base}`)).toBe(false);
      }
      // Deprecated `members` alias is also gone.
      expect(names.has('st_members')).toBe(false);
      expect(names.has('coord_members')).toBe(false);
    } finally {
      await close();
    }
  });

  it('st_msg_ls resolves + returns a structured result', async () => {
    const { client, close } = await connect();
    try {
      const r = (await client.callTool({
        name: 'st_msg_ls',
        arguments: {},
      })) as { structuredContent?: unknown };
      expect(r.structuredContent).toBeDefined();
    } finally {
      await close();
    }
  });
});
