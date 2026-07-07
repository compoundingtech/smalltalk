// tests/unit/mcp/shared.test.ts — cross-cutting MCP-layer tests.
//
// Concurrent calls, pre-command sweep regression, tools/list integrity,
// drift guard, identity plumbing.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { EXPECTED_TOOL_NAMES } from '../../../src/mcp/capabilities.ts';
import { createMcpServer } from '../../../src/mcp/index.ts';
import { errorCode, errorPayload } from "./_helpers.ts";
import { asIdentity } from '../../../src/types.ts';

let scratch: string;
let stRoot: string;
let client: Client;
let handle: ReturnType<typeof createMcpServer>;

beforeEach(async () => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-mcp-shared-'));
  stRoot = join(scratch, 'coord');
  for (const id of ['alice', 'bob']) {
    mkdirSync(join(stRoot, id, 'inbox'), { recursive: true });
    mkdirSync(join(stRoot, id, 'archive'), { recursive: true });
  }
  handle = createMcpServer({
    root: stRoot,
    identity: asIdentity('alice'),
  });
  client = new Client({ name: 'test-shared', version: '1.0' });
  const [c, s] = InMemoryTransport.createLinkedPair();
  await Promise.all([client.connect(c), handle.mcp.connect(s)]);
});
afterEach(async () => {
  await handle.close();
  rmSync(scratch, { recursive: true, force: true });
});

interface CallResult {
  isError?: boolean;
  content?: Array<{ type: string; text?: string }>;
  structuredContent?: Record<string, unknown>;
}

async function call(
  name: string,
  args: Record<string, unknown>
): Promise<CallResult> {
  return (await client.callTool({ name, arguments: args })) as CallResult;
}

// ─── tools/list integrity ──────────────────────────────────────────────

describe('shared — tools/list', () => {
  it('returns exactly the EXPECTED_TOOL_NAMES set, in any order', async () => {
    const r = await client.listTools();
    const names = r.tools.map((t) => t.name).sort();
    expect(names).toEqual([...EXPECTED_TOOL_NAMES].sort());
  });

  it('every tool advertises an inputSchema with type=object', async () => {
    const r = await client.listTools();
    for (const tool of r.tools) {
      expect(tool.inputSchema?.type).toBe('object');
    }
  });

  it('every tool advertises an outputSchema (structuredContent contract)', async () => {
    const r = await client.listTools();
    for (const tool of r.tools) {
      expect(tool.outputSchema).toBeDefined();
    }
  });

  it('every tool has a description string', async () => {
    const r = await client.listTools();
    for (const tool of r.tools) {
      expect(typeof tool.description).toBe('string');
      expect((tool.description ?? '').length).toBeGreaterThan(0);
    }
  });

  it('every tool has a title in annotations or as a top-level field', async () => {
    const r = await client.listTools();
    for (const tool of r.tools) {
      // SDK may surface the title at the top level (newer spec) or inside
      // annotations — either is fine.
      const title = tool.title ?? tool.annotations?.title;
      expect(typeof title).toBe('string');
    }
  });
});

// ─── Drift guard ──────────────────────────────────────────────────────

describe('shared — drift guard (capabilities + tool list)', () => {
  it('capabilities + tool list are identical before and after a sequence of tool calls', async () => {
    const before = {
      caps: client.getServerCapabilities(),
      version: client.getServerVersion(),
      tools: (await client.listTools()).tools.map((t) => t.name).sort(),
    };

    // Exercise multiple tools.
    await call('st_msg_send', { to: 'bob', body: 'msg1' });
    await call('st_msg_ls', {});
    await call('st_msg_send', { to: 'alice', body: 'msg2', from: 'bob' });

    const after = {
      caps: client.getServerCapabilities(),
      version: client.getServerVersion(),
      tools: (await client.listTools()).tools.map((t) => t.name).sort(),
    };

    expect(after).toEqual(before);
  });
});

// ─── Concurrency ──────────────────────────────────────────────────────

describe('shared — concurrent tool calls', () => {
  it('10 parallel st_msg_send calls produce 10 distinct files', async () => {
    const calls = Array.from({ length: 10 }, (_, i) =>
      call('st_msg_send', { to: 'bob', body: `msg-${i}` })
    );
    const results = await Promise.all(calls);
    const filenames = results
      .map((r) => r.structuredContent?.filename as string)
      .filter(Boolean);
    expect(filenames).toHaveLength(10);
    expect(new Set(filenames).size).toBe(10);
    expect(readdirSync(join(stRoot, 'bob', 'inbox'))).toHaveLength(10);
  });

  it('mixed parallel calls (send + ls + read) work without state corruption', async () => {
    // Pre-populate one message so read has something to find.
    const send0 = await call('st_msg_send', { to: 'alice', body: 'seed', from: 'bob' });
    const seedFn = send0.structuredContent?.filename as string;

    const results = await Promise.all([
      call('st_msg_send', { to: 'bob', body: 'a' }),
      call('st_msg_send', { to: 'bob', body: 'b' }),
      call('st_msg_ls', {}),
      call('st_msg_ls', { identity: 'bob' }),
      call('st_msg_read', { filename: seedFn }),
    ]);
    for (const r of results) {
      expect(r.isError).toBeUndefined();
    }
  });
});

// ─── No inline presweep, but lazy-read sweep cleans on st_msg_read ──

describe('shared — sweep is a convergence operation', () => {
  it('st_msg_ls does NOT presweep — zombie stays visible', async () => {
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(join(stRoot, 'alice', 'inbox', f), 'same');
    writeFileSync(join(stRoot, 'alice', 'archive', f), 'same');
    const r = await call('st_msg_ls', {});
    expect(r.isError).toBeUndefined();
    expect(r.structuredContent?.matches).toEqual([f]);
    expect(existsSync(join(stRoot, 'alice', 'inbox', f))).toBe(true);
    expect(existsSync(join(stRoot, 'alice', 'archive', f))).toBe(true);
  });

  it('st_msg_read lazy-sweeps the byte-identical inbox twin', async () => {
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(
      join(stRoot, 'alice', 'inbox', f),
      '---\nfrom: bob\n---\nbody\n'
    );
    writeFileSync(
      join(stRoot, 'alice', 'archive', f),
      '---\nfrom: bob\n---\nbody\n'
    );
    const r = await call('st_msg_read', { filename: f });
    expect(r.isError).toBeUndefined();
    // Lazy-read cleaned the inbox copy, archive stayed.
    expect(existsSync(join(stRoot, 'alice', 'inbox', f))).toBe(false);
    expect(existsSync(join(stRoot, 'alice', 'archive', f))).toBe(true);
  });
});

// ─── Identity plumbing ────────────────────────────────────────────────

describe('shared — identity plumbing', () => {
  it('bad ST_AGENT at server construction → every tool surfaces IDENTITY_NOT_HOSTED', async () => {
    // Tear down and rebuild against a missing identity.
    await handle.close();
    handle = createMcpServer({
      root: stRoot,
      identity: asIdentity('ghost'), // valid grammar but no folder on disk
    });
    client = new Client({ name: 'test-shared-ghost', version: '1.0' });
    const [c, s] = InMemoryTransport.createLinkedPair();
    await Promise.all([client.connect(c), handle.mcp.connect(s)]);

    const cases = [
      ['st_msg_send', { to: 'bob', body: 'm' }],
      ['st_msg_ls', {}],
      ['st_msg_read', { filename: '1714826789010-aaaaaa.md' }],
      ['st_msg_archive', { filename: '1714826789010-aaaaaa.md' }],
      ['st_msg_thread', { filename: '1714826789010-aaaaaa.md' }],
    ] as const;
    for (const [name, args] of cases) {
      const r = await call(name, args);
      expect(r.isError).toBe(true);
      expect(errorCode(r)).toBe('IDENTITY_NOT_HOSTED');
    }
  });
});

// ─── Error response shape regression ──────────────────────────────────

describe('shared — error response shape', () => {
  it('every CoordError surfaces with isError + content[0].text + _meta["coord/error"]', async () => {
    // Trigger one of each error class via different tools.
    const r1 = await call('st_msg_send', { to: 'INVALID', body: 'm' });
    expect(r1.isError).toBe(true);
    expect((r1.content?.[0] as { text: string } | undefined)?.text).toMatch(
      /^INVALID_IDENTITY:/
    );
    expect(errorPayload(r1)).toMatchObject({
      code: 'INVALID_IDENTITY',
    });
    expect(r1.structuredContent).toBeUndefined();

    const r2 = await call('st_msg_archive', { filename: 'garbage' });
    expect(r2.isError).toBe(true);
    expect(errorCode(r2)).toBe('INVALID_FILENAME');
    expect(r2.structuredContent).toBeUndefined();
  });
});
