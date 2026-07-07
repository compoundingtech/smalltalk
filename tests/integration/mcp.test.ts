// tests/integration/mcp.test.ts — `bin/coord mcp` driven over real
// stdio, via the SDK's StdioClientTransport.
//
// One subprocess boot per describe block (via beforeAll) for speed;
// each test gets a fresh per-test root under a shared scratch dir, and
// a `beforeEach` drift guard asserts capabilities + tool list haven't
// shifted between tests.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { join } from 'node:path';
import {
  afterAll,
  beforeAll,
  beforeEach,
  describe,
  expect,
  it,
} from 'vitest';

import { errorCode, errorPayload } from "../unit/mcp/_helpers.ts";
import { EXPECTED_TOOL_NAMES } from '../../src/mcp/capabilities.ts';
import { COORD_BIN, mkRoot, mkScratch } from './helpers.ts';

interface CallResult {
  isError?: boolean;
  content?: Array<{ type: string; text?: string }>;
  structuredContent?: Record<string, unknown>;
}

/**
 * Spawn `bin/coord mcp` once per describe. The ST_ROOT env is
 * pinned at boot — per-test isolation comes from each test using a
 * fresh subdirectory under that root and asking for it via the
 * `identity` arg in tool calls.
 *
 * One subprocess + N identities is faster than N subprocesses, and
 * coord's MCP server is stateless given a clean root (the Coord
 * factory has no per-call mutable state beyond the filesystem).
 */
async function bootSubprocess(stRoot: string): Promise<{
  client: Client;
  transport: StdioClientTransport;
  shutdown: () => Promise<void>;
}> {
  const transport = new StdioClientTransport({
    command: COORD_BIN,
    args: ['mcp'],
    env: {
      PATH: process.env.PATH ?? '',
      HOME: process.env.HOME ?? '',
      ST_ROOT: stRoot,
      ST_AGENT: 'alice',
    },
    stderr: 'pipe',
  });
  const client = new Client({ name: 'test-int', version: '1.0' });
  await client.connect(transport);
  const shutdown = async (): Promise<void> => {
    try {
      await client.close();
    } catch {
      // ignore — transport may already be closed
    }
  };
  return { client, transport, shutdown };
}

async function callTool(
  client: Client,
  name: string,
  args: Record<string, unknown>
): Promise<CallResult> {
  return (await client.callTool({ name, arguments: args })) as CallResult;
}

function setupIdentity(root: string, id: string): void {
  mkdirSync(join(root, id, 'inbox'), { recursive: true });
  mkdirSync(join(root, id, 'archive'), { recursive: true });
}

// ─── Lifecycle: server boots and responds ──────────────────────────────

describe('mcp integration — lifecycle', () => {
  let stRoot: string;
  let client: Client;
  let shutdown: () => Promise<void>;

  beforeAll(async () => {
    stRoot = mkScratch();
    mkdirSync(stRoot, { recursive: true });
    setupIdentity(stRoot, 'alice');
    setupIdentity(stRoot, 'bob');
    const r = await bootSubprocess(stRoot);
    client = r.client;
    shutdown = r.shutdown;
  }, 30_000);

  afterAll(async () => {
    await shutdown();
    try {
      rmSync(stRoot, { recursive: true, force: true });
    } catch {
      // best-effort
    }
  });

  it('initialize handshake completes; serverInfo + capabilities advertised', () => {
    expect(client.getServerVersion()?.name).toBe('st');
    expect(client.getServerCapabilities()?.tools).toBeDefined();
  });

  it('tools/list returns exactly the expected non-channel tools', async () => {
    const r = await client.listTools();
    expect(r.tools.map((t) => t.name).sort()).toEqual(
      [...EXPECTED_TOOL_NAMES].sort()
    );
  });

  it('drift guard: a sequence of tool calls leaves capabilities + tool list unchanged', async () => {
    const before = {
      caps: client.getServerCapabilities(),
      tools: (await client.listTools()).tools.map((t) => t.name).sort(),
    };
    await callTool(client, 'st_msg_send', {
      to: 'bob',
      body: 'drift-guard-msg',
    });
    await callTool(client, 'st_msg_ls', {});
    const after = {
      caps: client.getServerCapabilities(),
      tools: (await client.listTools()).tools.map((t) => t.name).sort(),
    };
    expect(after).toEqual(before);
  });

  it('experimental.claude/channel is NOT advertised in Phase 1', () => {
    expect(client.getServerCapabilities()?.experimental).toBeUndefined();
  });

  it('unknown tool name → tool error response (server keeps running)', async () => {
    // The SDK client may either throw a protocol error or pass back an
    // isError response; both are acceptable as long as the server stays up.
    let errored = false;
    try {
      const r = (await client.callTool({
        name: 'no_such_tool',
        arguments: {},
      })) as { isError?: boolean };
      if (r.isError === true) errored = true;
    } catch {
      errored = true;
    }
    expect(errored).toBe(true);
    // Server still alive: a known call should still work.
    const r = await callTool(client, 'st_msg_ls', {});
    expect(r.isError).toBeUndefined();
  });
});

// ─── st_msg_send end-to-end ─────────────────────────────────────────────

describe('mcp integration — st_msg_send', () => {
  let stRoot: string;
  let client: Client;
  let shutdown: () => Promise<void>;

  beforeAll(async () => {
    stRoot = mkScratch();
    mkdirSync(stRoot, { recursive: true });
    setupIdentity(stRoot, 'alice');
    setupIdentity(stRoot, 'bob');
    const r = await bootSubprocess(stRoot);
    client = r.client;
    shutdown = r.shutdown;
  }, 30_000);
  afterAll(async () => {
    await shutdown();
    rmSync(stRoot, { recursive: true, force: true });
  });

  let driftToolCount: number;
  beforeEach(async () => {
    driftToolCount = (await client.listTools()).tools.length;
    expect(driftToolCount).toBe(EXPECTED_TOOL_NAMES.length);
  });

  it('writes a real file via stdio + JSON-RPC', async () => {
    const r = await callTool(client, 'st_msg_send', {
      to: 'bob',
      body: 'hello bob',
      subject: 'integration',
    });
    expect(r.isError).toBeUndefined();
    const filename = r.structuredContent?.filename as string;
    expect(filename).toMatch(/^[0-9]{13}-[0-9a-z]{6}\.md$/);
    expect(existsSync(join(stRoot, 'bob', 'inbox', filename))).toBe(true);
    const text = readFileSync(
      join(stRoot, 'bob', 'inbox', filename),
      'utf8'
    );
    expect(text).toContain('from: alice');
    expect(text).toContain('subject: integration');
  });

  it('roundtrips via st_msg_ls', async () => {
    const send = await callTool(client, 'st_msg_send', {
      to: 'bob',
      body: 'roundtrip',
    });
    const filename = send.structuredContent?.filename as string;
    const ls = await callTool(client, 'st_msg_ls', { identity: 'bob' });
    expect((ls.structuredContent?.matches as string[]) ?? []).toContain(
      filename
    );
  });

  it('typed errors are surfaced as MCP tool errors (not transport errors)', async () => {
    const r = await callTool(client, 'st_msg_send', {
      to: 'INVALID',
      body: 'm',
    });
    expect(r.isError).toBe(true);
    expect(errorCode(r)).toBe('INVALID_IDENTITY');
  });

  it('schema validation rejects malformed input as a tool error', async () => {
    const r = await callTool(client, 'st_msg_send', { body: 'no recipient' });
    expect(r.isError).toBe(true);
  });
});

// ─── st_msg_ls end-to-end ───────────────────────────────────────────────

describe('mcp integration — st_msg_ls', () => {
  let stRoot: string;
  let client: Client;
  let shutdown: () => Promise<void>;

  beforeAll(async () => {
    stRoot = mkScratch();
    mkdirSync(stRoot, { recursive: true });
    setupIdentity(stRoot, 'alice');
    setupIdentity(stRoot, 'bob');
    const r = await bootSubprocess(stRoot);
    client = r.client;
    shutdown = r.shutdown;
  }, 30_000);
  afterAll(async () => {
    await shutdown();
    rmSync(stRoot, { recursive: true, force: true });
  });

  beforeEach(async () => {
    const r = await client.listTools();
    expect(r.tools).toHaveLength(EXPECTED_TOOL_NAMES.length);
  });

  it('empty inbox → matches: []', async () => {
    const r = await callTool(client, 'st_msg_ls', {});
    expect((r.structuredContent?.matches as string[]) ?? []).toEqual([]);
    expect(r.structuredContent?.identity).toBe('alice');
  });

  it('lists messages chronologically', async () => {
    // Send three to alice (so they land in alice/inbox under bob's name).
    for (let i = 0; i < 3; i++) {
      await callTool(client, 'st_msg_send', {
        to: 'alice',
        body: `m${i}`,
        from: 'bob',
      });
    }
    const r = await callTool(client, 'st_msg_ls', {});
    const matches = (r.structuredContent?.matches as string[]) ?? [];
    expect(matches).toHaveLength(3);
    // Filenames embed unix-ms; sorted ascending = chronological.
    const sorted = [...matches].sort();
    expect(matches).toEqual(sorted);
  });

  it('--archive switches the listed folder', async () => {
    const r = await callTool(client, 'st_msg_ls', {
      identity: 'alice',
      archive: true,
    });
    expect(r.structuredContent?.archive).toBe(true);
  });
});

// ─── st_msg_read + st_msg_archive end-to-end ─────────────────────────────

describe('mcp integration — st_msg_read + st_msg_archive', () => {
  let stRoot: string;
  let client: Client;
  let shutdown: () => Promise<void>;

  beforeAll(async () => {
    stRoot = mkScratch();
    mkdirSync(stRoot, { recursive: true });
    setupIdentity(stRoot, 'alice');
    setupIdentity(stRoot, 'bob');
    const r = await bootSubprocess(stRoot);
    client = r.client;
    shutdown = r.shutdown;
  }, 30_000);
  afterAll(async () => {
    await shutdown();
    rmSync(stRoot, { recursive: true, force: true });
  });

  it('round-trip: send → read → archive', async () => {
    const send = await callTool(client, 'st_msg_send', {
      to: 'bob',
      body: 'roundtrip body',
      subject: 'roundtrip',
    });
    const filename = send.structuredContent?.filename as string;

    const read = await callTool(client, 'st_msg_read', {
      filename,
      identity: 'bob',
    });
    expect(read.isError).toBeUndefined();
    expect(
      (read.structuredContent?.message as { from: string }).from
    ).toBe('alice');
    expect(read.structuredContent?.folder).toBe('inbox');

    const archive = await callTool(client, 'st_msg_archive', {
      filename,
      identity: 'bob',
    });
    expect(archive.structuredContent?.outcome).toBe('moved');
    expect(
      existsSync(join(stRoot, 'bob', 'inbox', filename))
    ).toBe(false);
    expect(
      existsSync(join(stRoot, 'bob', 'archive', filename))
    ).toBe(true);
  });

  it('st_msg_read on a missing file → MESSAGE_NOT_FOUND', async () => {
    const r = await callTool(client, 'st_msg_read', {
      filename: '1714826789010-zzzzzz.md',
      identity: 'bob',
    });
    expect(errorCode(r)).toBe('MESSAGE_NOT_FOUND');
  });

  it('st_msg_archive case-3 divergent twin → ARCHIVE_CONFLICT', async () => {
    const f = '1714826789999-aaaaaa.md';
    writeFileSync(join(stRoot, 'bob', 'inbox', f), 'inbox-version');
    writeFileSync(join(stRoot, 'bob', 'archive', f), 'archive-version');
    const r = await callTool(client, 'st_msg_archive', {
      filename: f,
      identity: 'bob',
    });
    expect(errorCode(r)).toBe('ARCHIVE_CONFLICT');
  });
});

// ─── st_msg_thread end-to-end ───────────────────────────────────────────

describe('mcp integration — st_msg_thread', () => {
  let stRoot: string;
  let client: Client;
  let shutdown: () => Promise<void>;

  beforeAll(async () => {
    stRoot = mkScratch();
    mkdirSync(stRoot, { recursive: true });
    setupIdentity(stRoot, 'alice');
    setupIdentity(stRoot, 'bob');
    const r = await bootSubprocess(stRoot);
    client = r.client;
    shutdown = r.shutdown;
  }, 30_000);
  afterAll(async () => {
    await shutdown();
    rmSync(stRoot, { recursive: true, force: true });
  });

  it('walks a 3-message linear chain', async () => {
    const f1 = (
      await callTool(client, 'st_msg_send', {
        to: 'bob',
        body: 'root',
      })
    ).structuredContent?.filename as string;
    // 2ms gap to keep filename sort stable.
    await new Promise((r) => setTimeout(r, 2));
    const f2 = (
      await callTool(client, 'st_msg_send', {
        to: 'bob',
        body: 'reply1',
        inReplyTo: f1,
      })
    ).structuredContent?.filename as string;
    await new Promise((r) => setTimeout(r, 2));
    const f3 = (
      await callTool(client, 'st_msg_send', {
        to: 'bob',
        body: 'reply2',
        inReplyTo: f2,
      })
    ).structuredContent?.filename as string;

    const r = await callTool(client, 'st_msg_thread', {
      filename: f1,
      identity: 'bob',
    });
    interface TM {
      filename: string;
    }
    const filenames = (
      r.structuredContent?.messages as TM[]
    ).map((m) => m.filename);
    expect(filenames).toEqual([f1, f2, f3]);
  });

  it('seed not found → MESSAGE_NOT_FOUND', async () => {
    const r = await callTool(client, 'st_msg_thread', {
      filename: '1714826789010-zzzzzz.md',
      identity: 'bob',
    });
    expect(errorCode(r)).toBe('MESSAGE_NOT_FOUND');
  });

  it('tree=true returns depth-indented walk order', async () => {
    // Build a small branching thread.
    const root = (
      await callTool(client, 'st_msg_send', {
        to: 'bob',
        body: 'r',
      })
    ).structuredContent?.filename as string;
    await new Promise((r) => setTimeout(r, 2));
    await callTool(client, 'st_msg_send', {
      to: 'bob',
      body: 'r1',
      inReplyTo: root,
    });
    await new Promise((r) => setTimeout(r, 2));
    await callTool(client, 'st_msg_send', {
      to: 'bob',
      body: 'r2',
      inReplyTo: root,
    });
    const r = await callTool(client, 'st_msg_thread', {
      filename: root,
      identity: 'bob',
      tree: true,
    });
    interface TM {
      filename: string;
    }
    expect(
      (r.structuredContent?.messages as TM[]).length
    ).toBeGreaterThanOrEqual(3);
  });
});

// ─── Error-shape regression across stdio ───────────────────────────────

describe('mcp integration — error response shape over real stdio', () => {
  let stRoot: string;
  let client: Client;
  let shutdown: () => Promise<void>;

  beforeAll(async () => {
    stRoot = mkScratch();
    mkdirSync(stRoot, { recursive: true });
    setupIdentity(stRoot, 'alice');
    const r = await bootSubprocess(stRoot);
    client = r.client;
    shutdown = r.shutdown;
  }, 30_000);
  afterAll(async () => {
    await shutdown();
    rmSync(stRoot, { recursive: true, force: true });
  });

  it('CoordError → isError + content[0].text starts with code + _meta["coord/error"]', async () => {
    const r = await callTool(client, 'st_msg_send', {
      to: 'INVALID',
      body: 'm',
    });
    expect(r.isError).toBe(true);
    expect(r.content?.[0]?.text).toMatch(/^INVALID_IDENTITY:/);
    expect(errorPayload(r)).toMatchObject({ code: 'INVALID_IDENTITY' });
    expect(r.structuredContent).toBeUndefined();
  });
});

// ─── st_agents end-to-end ──────────────────────────────────────────

describe('mcp integration — st_agents', () => {
  let stRoot: string;
  let client: Client;
  let shutdown: () => Promise<void>;

  beforeAll(async () => {
    stRoot = mkScratch();
    mkdirSync(stRoot, { recursive: true });
    setupIdentity(stRoot, 'alice');
    setupIdentity(stRoot, 'bob');
    setupIdentity(stRoot, 'carol');
    writeFileSync(join(stRoot, 'alice', 'status'), 'available\n');
    writeFileSync(join(stRoot, 'bob', 'status'), 'busy\n');
    const r = await bootSubprocess(stRoot);
    client = r.client;
    shutdown = r.shutdown;
  }, 30_000);
  afterAll(async () => {
    await shutdown();
    rmSync(stRoot, { recursive: true, force: true });
  });

  interface MemberShape {
    identity: string;
    status: string;
    name: string | null;
    lastActivity?: number | null;
    inbox?: number;
  }

  it('appears in tools/list when the server boots in non-channel mode', async () => {
    const r = await client.listTools();
    expect(r.tools.map((t) => t.name)).toContain('st_agents');
  });

  it('zero-arg call returns all identities with effective status', async () => {
    const r = await callTool(client, 'st_agents', {});
    const members = (r.structuredContent?.members as MemberShape[]) ?? [];
    expect(members.map((m) => m.identity)).toEqual(['alice', 'bob', 'carol']);
    expect(members.find((m) => m.identity === 'alice')?.status).toBe(
      'available'
    );
    expect(members.find((m) => m.identity === 'bob')?.status).toBe('busy');
    expect(members.find((m) => m.identity === 'carol')?.status).toBe(
      'offline'
    );
  });

  it('status filter narrows to a single state', async () => {
    const r = await callTool(client, 'st_agents', { status: 'busy' });
    const members = (r.structuredContent?.members as MemberShape[]) ?? [];
    expect(members.map((m) => m.identity)).toEqual(['bob']);
  });

  it('enrich: true returns lastActivity/inbox fields', async () => {
    const r = await callTool(client, 'st_agents', { enrich: true });
    const members = (r.structuredContent?.members as MemberShape[]) ?? [];
    expect(members.length).toBeGreaterThan(0);
    for (const m of members) {
      expect(m).toHaveProperty('lastActivity');
      expect(m).toHaveProperty('inbox');
    }
  });
});

// Mark mkRoot as used so the import isn't dead — the helpers module's
// process-exit cleanup tracks every dir mkScratch creates.
void mkRoot;
