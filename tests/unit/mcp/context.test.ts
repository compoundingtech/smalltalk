// tests/unit/mcp/context.test.ts — brief-024 context/ v1 MCP tools.
//
// Every context_* verb is dual-registered under coord_* AND st_*
// (brief-005-phase0 §3). We exercise the coord_* side end-to-end here
// and spot-check that the st_* alias is registered; a fuller
// tools/list-matches-EXPECTED regression lives elsewhere.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { createMcpServer } from '../../../src/mcp/index.ts';
import { asIdentity } from '../../../src/types.ts';

let scratch: string;
let coordRoot: string;
let client: Client;
let handle: ReturnType<typeof createMcpServer>;

beforeEach(async () => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-mcp-context-'));
  coordRoot = join(scratch, 'coord');
  for (const id of ['alice', 'bob']) {
    mkdirSync(join(coordRoot, id, 'inbox'), { recursive: true });
    mkdirSync(join(coordRoot, id, 'archive'), { recursive: true });
  }
  handle = createMcpServer({
    root: coordRoot,
    identity: asIdentity('alice'),
  });
  client = new Client({ name: 'test-context', version: '1.0' });
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

async function callRead(args: Record<string, unknown>): Promise<CallResult> {
  return (await client.callTool({
    name: 'coord_context_read',
    arguments: args,
  })) as CallResult;
}
async function callWrite(args: Record<string, unknown>): Promise<CallResult> {
  return (await client.callTool({
    name: 'coord_context_write',
    arguments: args,
  })) as CallResult;
}
async function callAppend(args: Record<string, unknown>): Promise<CallResult> {
  return (await client.callTool({
    name: 'coord_context_append',
    arguments: args,
  })) as CallResult;
}

// ─── Registration ────────────────────────────────────────────────────────

describe('context_* — tools/list registration', () => {
  it('registers all three verbs under both coord_ and st_ prefixes', async () => {
    const r = await client.listTools();
    const names = new Set(r.tools.map((t) => t.name));
    for (const base of ['context_read', 'context_write', 'context_append']) {
      expect(names.has(`coord_${base}`)).toBe(true);
      expect(names.has(`st_${base}`)).toBe(true);
    }
  });

  it('context_read: identity + file are both optional', async () => {
    const r = await client.listTools();
    const tool = r.tools.find((t) => t.name === 'coord_context_read');
    expect(tool).toBeDefined();
    // Nothing is required — that's the absent-able contract.
    expect(tool?.inputSchema?.required ?? []).toEqual([]);
  });

  it('context_write requires body', async () => {
    const r = await client.listTools();
    const tool = r.tools.find((t) => t.name === 'coord_context_write');
    expect(tool?.inputSchema?.required).toEqual(['body']);
  });

  it('context_append requires decision + why', async () => {
    const r = await client.listTools();
    const tool = r.tools.find((t) => t.name === 'coord_context_append');
    expect(new Set(tool?.inputSchema?.required ?? [])).toEqual(
      new Set(['decision', 'why'])
    );
  });
});

// ─── Absent-able (the load-bearing property) ─────────────────────────────

describe('coord_context_read — absent-able', () => {
  it("cold agent (no context/ folder) → text is empty, absent: true, and the folder is NOT created", async () => {
    const r = await callRead({});
    expect(r.isError).toBeUndefined();
    expect(r.structuredContent).toMatchObject({
      identity: 'alice',
      file: 'now',
      text: '',
      absent: true,
    });
    // A read must not create the folder — the eval's control arm needs
    // this to stay absent so the "no context" scenario is testable.
    expect(existsSync(join(coordRoot, 'alice', 'context'))).toBe(false);
  });

  it('summary line reflects the absent state', async () => {
    const r = await callRead({});
    expect(r.content?.[0]?.text).toMatch(/absent/);
  });

  it('--full on cold agent → still empty + absent', async () => {
    const r = await callRead({ file: 'full' });
    expect(r.structuredContent).toMatchObject({
      file: 'full',
      text: '',
      absent: true,
    });
  });
});

// ─── write + roundtrip ───────────────────────────────────────────────────

describe('coord_context_write', () => {
  it('creates the context/ folder + writes now.md', async () => {
    const r = await callWrite({ body: 'brief-024 v1' });
    expect(r.isError).toBeUndefined();
    const path = r.structuredContent?.path as string;
    expect(path).toContain('/alice/context/now.md');
    expect(readFileSync(path, 'utf8')).toBe('brief-024 v1\n');
  });

  it('read-after-write round-trips exactly', async () => {
    await callWrite({ body: 'the current state\n' });
    const r = await callRead({});
    expect(r.structuredContent).toMatchObject({
      identity: 'alice',
      file: 'now',
      text: 'the current state\n',
      absent: false,
    });
  });

  it('write rejects a missing body', async () => {
    const r = await callWrite({});
    expect(r.isError).toBe(true);
  });
});

// ─── append + read ───────────────────────────────────────────────────────

describe('coord_context_append', () => {
  it('creates one file per entry; returns the filename + line', async () => {
    const r = await callAppend({
      decision: 'ship v1 without hook legs',
      why: 'iterate on schema before wiring hooks',
      timestamp: '2026-07-02T22:30:00.000Z',
    });
    expect(r.isError).toBeUndefined();
    const line = r.structuredContent?.line as string;
    expect(line).toBe(
      '- 2026-07-02T22:30:00.000Z ship v1 without hook legs. why: iterate on schema before wiring hooks.'
    );
    // Filename shape: LAYOUT-004 <unix-ms>-<rand6>.md — same as message
    // inboxes. The MCP surface exposes both filename and full path.
    const filename = r.structuredContent?.filename as string;
    expect(filename).toMatch(/^\d+-[0-9a-z]{6}\.md$/);
    const path = r.structuredContent?.path as string;
    expect(path).toContain('/context/decisions/');
    expect(path.endsWith(filename)).toBe(true);
  });

  it('two appends land in two distinct files; read --decisions shows both in chrono order', async () => {
    await callAppend({
      decision: 'a',
      why: 'one',
      timestamp: '2026-07-02T00:00:00.000Z',
    });
    await callAppend({
      decision: 'b',
      why: 'two',
      timestamp: '2026-07-02T00:01:00.000Z',
    });
    const r = await callRead({ file: 'decisions' });
    const text = r.structuredContent?.text as string;
    const aIdx = text.indexOf('a. why: one.');
    const bIdx = text.indexOf('b. why: two.');
    expect(aIdx).toBeGreaterThanOrEqual(0);
    // Earlier timestamp → earlier in the concatenated output.
    expect(bIdx).toBeGreaterThan(aIdx);
  });

  it('append requires decision + why (schema-level)', async () => {
    const missingWhy = await callAppend({ decision: 'x' });
    expect(missingWhy.isError).toBe(true);
    const missingDecision = await callAppend({ why: 'y' });
    expect(missingDecision.isError).toBe(true);
  });
});

// ─── Cross-identity reads ────────────────────────────────────────────────

describe("coord_context_read — reading a peer's context", () => {
  it("explicit identity reads bob's context/now.md", async () => {
    // Populate bob directly via the write tool with an explicit identity
    // override — proves the identity plumbing goes end-to-end.
    await callWrite({ body: "bob's state", identity: 'bob' });
    const r = await callRead({ identity: 'bob' });
    expect(r.structuredContent).toMatchObject({
      identity: 'bob',
      file: 'now',
      text: "bob's state\n",
      absent: false,
    });
  });
});

// ─── st_ alias smoke test ────────────────────────────────────────────────

describe('st_context_* — dual-registration works end-to-end', () => {
  it('st_context_write + st_context_read roundtrip identically to coord_*', async () => {
    await client.callTool({
      name: 'st_context_write',
      arguments: { body: 'via st alias' },
    });
    const r = (await client.callTool({
      name: 'st_context_read',
      arguments: {},
    })) as CallResult;
    expect(r.structuredContent?.text).toBe('via st alias\n');
  });
});
