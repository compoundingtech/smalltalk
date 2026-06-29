// tests/unit/aliases.test.ts — brief-005-phase0 acceptance items.
//
// Covers four of the six Phase 0 acceptance criteria as unit tests:
//   2. MCP server announces under the right name (coord vs st).
//   3. Both `coord_*` and `st_*` tool names resolve to the same handler.
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
  _resetCoordFallbackWarnings,
  canonicalServerName,
  coordRootFrom,
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
  _resetCoordFallbackWarnings();
});
afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
  vi.restoreAllMocks();
});

// ─── Item 4: env-var dual-honor ───────────────────────────────────────────

describe('env-var dual-honor (ST_* preferred over COORD_*)', () => {
  it('coordRootFrom: ST_ROOT wins when both are set', () => {
    const env = {
      ST_ROOT: '/st/path',
      COORD_ROOT: '/coord/path',
    } as NodeJS.ProcessEnv;
    expect(coordRootFrom(env)).toBe('/st/path');
  });

  it('coordRootFrom: COORD_ROOT used as fallback when ST_ROOT unset', () => {
    const env = { COORD_ROOT: '/coord/path' } as NodeJS.ProcessEnv;
    expect(coordRootFrom(env)).toBe('/coord/path');
  });

  it('envIdentityFrom: ST_IDENTITY wins when both are set', () => {
    const env = {
      ST_IDENTITY: 'st-bob',
      COORD_IDENTITY: 'coord-bob',
    } as NodeJS.ProcessEnv;
    expect(envIdentityFrom(env)).toBe('st-bob');
  });

  it('envIdentityFrom: COORD_IDENTITY used as fallback', () => {
    const env = { COORD_IDENTITY: 'coord-bob' } as NodeJS.ProcessEnv;
    expect(envIdentityFrom(env)).toBe('coord-bob');
  });

  it('envIdentityFrom: neither set → undefined (caller throws own error)', () => {
    expect(envIdentityFrom({} as NodeJS.ProcessEnv)).toBeUndefined();
  });

  it('warns ONCE per process when COORD_ROOT is honored', () => {
    const spy = vi.spyOn(process.stderr, 'write').mockReturnValue(true);
    const env = { COORD_ROOT: '/coord/path' } as NodeJS.ProcessEnv;
    coordRootFrom(env);
    coordRootFrom(env);
    coordRootFrom(env);
    const warnCalls = spy.mock.calls.filter((c) =>
      String(c[0]).includes('COORD_ROOT')
    );
    expect(warnCalls).toHaveLength(1);
    expect(String(warnCalls[0]![0])).toContain('migrate to ST_ROOT');
  });

  it('warns ONCE per process when COORD_IDENTITY is honored', () => {
    const spy = vi.spyOn(process.stderr, 'write').mockReturnValue(true);
    const env = { COORD_IDENTITY: 'coord-bob' } as NodeJS.ProcessEnv;
    envIdentityFrom(env);
    envIdentityFrom(env);
    const warnCalls = spy.mock.calls.filter((c) =>
      String(c[0]).includes('COORD_IDENTITY')
    );
    expect(warnCalls).toHaveLength(1);
    expect(String(warnCalls[0]![0])).toContain('migrate to ST_AGENT');
  });

  it('no warning when ST_ROOT is set (no fallback)', () => {
    const spy = vi.spyOn(process.stderr, 'write').mockReturnValue(true);
    coordRootFrom({ ST_ROOT: '/st' } as NodeJS.ProcessEnv);
    const warnCalls = spy.mock.calls.filter((c) =>
      String(c[0]).includes('honoring COORD_')
    );
    expect(warnCalls).toHaveLength(0);
  });

  it('resolveIdentity: ST_IDENTITY preferred over COORD_IDENTITY', () => {
    // Real folder under our scratch HOME for the auto-create path.
    const root = join(scratch, 'state');
    mkdirSync(root, { recursive: true });
    const env = {
      ST_IDENTITY: 'st-claude',
      COORD_IDENTITY: 'coord-claude',
    } as NodeJS.ProcessEnv;
    expect(resolveIdentity({ env, coordRoot: root })).toBe('st-claude');
  });

  it('resolveIdentity: ST_AGENT preferred over ST_IDENTITY (brief-009 item 3)', () => {
    const root = join(scratch, 'state');
    mkdirSync(root, { recursive: true });
    const env = {
      ST_AGENT: 'st-agent',
      ST_IDENTITY: 'st-identity',
      COORD_IDENTITY: 'coord-identity',
    } as NodeJS.ProcessEnv;
    expect(resolveIdentity({ env, coordRoot: root })).toBe('st-agent');
  });

  it('resolveIdentity: three-level fallback ST_AGENT > ST_IDENTITY > COORD_IDENTITY', () => {
    const root = join(scratch, 'state');
    mkdirSync(root, { recursive: true });
    // Only COORD_IDENTITY set → it gets honored.
    const e1 = { COORD_IDENTITY: 'coord-only' } as NodeJS.ProcessEnv;
    expect(resolveIdentity({ env: e1, coordRoot: root })).toBe('coord-only');
    // ST_IDENTITY but no ST_AGENT → ST_IDENTITY wins.
    const e2 = {
      ST_IDENTITY: 'st-identity',
      COORD_IDENTITY: 'coord-id',
    } as NodeJS.ProcessEnv;
    expect(resolveIdentity({ env: e2, coordRoot: root })).toBe('st-identity');
    // All three set → ST_AGENT wins.
    const e3 = {
      ST_AGENT: 'st-agent',
      ST_IDENTITY: 'st-identity',
      COORD_IDENTITY: 'coord-id',
    } as NodeJS.ProcessEnv;
    expect(resolveIdentity({ env: e3, coordRoot: root })).toBe('st-agent');
  });

  it('envIdentityFrom: warns once when honoring ST_IDENTITY (post brief-009 item 3)', () => {
    const spy = vi.spyOn(process.stderr, 'write').mockReturnValue(true);
    const env = { ST_IDENTITY: 'st-identity-bob' } as NodeJS.ProcessEnv;
    envIdentityFrom(env);
    envIdentityFrom(env);
    const warnCalls = spy.mock.calls.filter((c) =>
      String(c[0]).includes('ST_IDENTITY')
    );
    expect(warnCalls).toHaveLength(1);
    expect(String(warnCalls[0]![0])).toContain('migrate to ST_AGENT');
  });
});

// ─── Item 5: state-dir resolution ─────────────────────────────────────────

describe('state-dir resolution (~/.local/state/smalltalk vs /coord)', () => {
  function withFakeHome(setup: (home: string) => void): string {
    const home = join(scratch, 'home');
    mkdirSync(home, { recursive: true });
    setup(home);
    return coordRootFrom({ HOME: home } as NodeJS.ProcessEnv);
  }

  it('only smalltalk/ exists → use smalltalk/', () => {
    const r = withFakeHome((home) => {
      mkdirSync(join(home, '.local/state/smalltalk'), { recursive: true });
    });
    expect(r).toMatch(/\.local\/state\/smalltalk$/);
  });

  it('only coord/ exists → fall back to coord/', () => {
    const r = withFakeHome((home) => {
      mkdirSync(join(home, '.local/state/coord'), { recursive: true });
    });
    expect(r).toMatch(/\.local\/state\/coord$/);
  });

  it('BOTH exist → silently prefer smalltalk/ (per pty-claude Nit 2)', () => {
    const r = withFakeHome((home) => {
      mkdirSync(join(home, '.local/state/smalltalk'), { recursive: true });
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
      coordRootFrom({ ST_ROOT: '/explicit', HOME: scratch } as NodeJS.ProcessEnv)
    ).toBe('/explicit');
  });
});

// ─── invokedAs + canonical-server-name helpers ────────────────────────────

describe('invokedAsFrom + canonicalServerName', () => {
  it('reads _ST_INVOKED_AS and accepts the three canonical names', () => {
    expect(invokedAsFrom({ _ST_INVOKED_AS: 'coord' } as NodeJS.ProcessEnv)).toBe(
      'coord'
    );
    expect(invokedAsFrom({ _ST_INVOKED_AS: 'st' } as NodeJS.ProcessEnv)).toBe(
      'st'
    );
    expect(
      invokedAsFrom({ _ST_INVOKED_AS: 'smalltalk' } as NodeJS.ProcessEnv)
    ).toBe('smalltalk');
  });

  it('defaults to coord when env var is absent or unknown', () => {
    expect(invokedAsFrom({} as NodeJS.ProcessEnv)).toBe('coord');
    expect(invokedAsFrom({ _ST_INVOKED_AS: 'nope' } as NodeJS.ProcessEnv)).toBe(
      'coord'
    );
  });

  it('canonicalServerName: coord → coord; st/smalltalk → st', () => {
    expect(canonicalServerName('coord')).toBe('coord');
    expect(canonicalServerName('st')).toBe('st');
    expect(canonicalServerName('smalltalk')).toBe('st');
  });
});

// ─── Item 2: MCP server name dual-registration ────────────────────────────

describe('MCP server name (Item 2)', () => {
  it('buildServerInfo returns the name passed in', () => {
    expect(buildServerInfo('coord').name).toBe('coord');
    expect(buildServerInfo('st').name).toBe('st');
  });

  it('version is preserved across both names', () => {
    expect(buildServerInfo('coord').version).toBe(
      buildServerInfo('st').version
    );
  });

  it('server announces "st" when serverName: "st" passed to createMcpServer', async () => {
    const root = join(scratch, 'state');
    mkdirSync(join(root, 'tester', 'inbox'), { recursive: true });
    mkdirSync(join(root, 'tester', 'archive'), { recursive: true });
    const handle = createMcpServer({
      root,
      identity: asIdentity('tester'),
      serverName: 'st',
    });
    const [a, b] = InMemoryTransport.createLinkedPair();
    const client = new Client({ name: 'aliases-test', version: '0' });
    await Promise.all([client.connect(a), handle.mcp.connect(b)]);
    try {
      // serverVersion includes name + version.
      const info = client.getServerVersion();
      expect(info?.name).toBe('st');
    } finally {
      await client.close();
      await handle.close();
    }
  });

  it('server announces "coord" by default (back-compat)', async () => {
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
      expect(client.getServerVersion()?.name).toBe('coord');
    } finally {
      await client.close();
      await handle.close();
    }
  });
});

// ─── Item 3: tool name dual-registration ──────────────────────────────────

describe('MCP tool name dual-registration (Item 3)', () => {
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

  it('listTools includes BOTH the coord_* and st_* registrations', async () => {
    const { client, close } = await connect();
    try {
      const r = await client.listTools();
      const names = new Set(r.tools.map((t) => t.name));
      // Spot-check both prefixes for every base name.
      for (const base of [
        'msg_send',
        'msg_ls',
        'msg_read',
        'msg_archive',
        'msg_thread',
        'members',
      ]) {
        expect(names.has(`coord_${base}`)).toBe(true);
        expect(names.has(`st_${base}`)).toBe(true);
      }
    } finally {
      await close();
    }
  });

  it('coord_msg_ls and st_msg_ls produce identical results', async () => {
    const { client, close } = await connect();
    try {
      const r1 = (await client.callTool({
        name: 'coord_msg_ls',
        arguments: {},
      })) as { structuredContent?: unknown };
      const r2 = (await client.callTool({
        name: 'st_msg_ls',
        arguments: {},
      })) as { structuredContent?: unknown };
      expect(r2.structuredContent).toEqual(r1.structuredContent);
    } finally {
      await close();
    }
  });

  it('coord_members and st_members are wired to the same handler', async () => {
    const { client, close } = await connect();
    try {
      const r1 = (await client.callTool({
        name: 'coord_members',
        arguments: {},
      })) as { structuredContent?: unknown };
      const r2 = (await client.callTool({
        name: 'st_members',
        arguments: {},
      })) as { structuredContent?: unknown };
      expect(r2.structuredContent).toEqual(r1.structuredContent);
    } finally {
      await close();
    }
  });
});
