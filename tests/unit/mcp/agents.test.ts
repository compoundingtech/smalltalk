// tests/unit/mcp/members.test.ts — st_agents tool, in-memory.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { createMcpServer } from '../../../src/mcp/index.ts';
import { asIdentity } from '../../../src/types.ts';

let scratch: string;
let coordRoot: string;
let client: Client;
let handle: ReturnType<typeof createMcpServer>;

function setupIdentity(id: string): void {
  mkdirSync(join(coordRoot, id, 'inbox'), { recursive: true });
  mkdirSync(join(coordRoot, id, 'archive'), { recursive: true });
}

function setStatus(id: string, status: string): void {
  writeFileSync(join(coordRoot, id, 'status'), `${status}\n`);
}

async function boot(identity = 'alice'): Promise<void> {
  handle = createMcpServer({
    root: coordRoot,
    identity: asIdentity(identity),
  });
  client = new Client({ name: 'test-members', version: '1.0' });
  const [c, s] = InMemoryTransport.createLinkedPair();
  await Promise.all([client.connect(c), handle.mcp.connect(s)]);
}

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-mcp-members-'));
  coordRoot = join(scratch, 'coord');
});

afterEach(async () => {
  if (handle) await handle.close();
  rmSync(scratch, { recursive: true, force: true });
});

interface CallResult {
  isError?: boolean;
  content?: Array<{ type: string; text?: string }>;
  structuredContent?: Record<string, unknown>;
}

interface MemberShape {
  identity: string;
  status: string;
  name: string | null;
  lastActivity?: number | null;
  inbox?: number;
}

async function call(args: Record<string, unknown> = {}): Promise<CallResult> {
  return (await client.callTool({
    name: 'st_agents',
    arguments: args,
  })) as CallResult;
}

// ─── tools/list registration ───────────────────────────────────────────

describe('st_agents — tools/list registration', () => {
  it('appears in tools/list with input + output schemas', async () => {
    setupIdentity('alice');
    await boot();
    const r = await client.listTools();
    const tool = r.tools.find((t) => t.name === 'st_agents');
    expect(tool).toBeDefined();
    expect(tool?.inputSchema).toBeDefined();
    expect(tool?.outputSchema).toBeDefined();
  });

  it('description contains the load-bearing peer-discovery phrase (checked on canonical st_agents)', async () => {
    setupIdentity('alice');
    await boot();
    const r = await client.listTools();
    const tool = r.tools.find((t) => t.name === 'st_agents');
    expect(tool?.description).toContain('Useful for peer discovery before sending');
  });

  it('st_agents is also registered (canonical name post-brief-009 item 3)', async () => {
    setupIdentity('alice');
    await boot();
    const r = await client.listTools();
    const names = r.tools.map((t) => t.name);
    expect(names).toContain('st_agents');
    expect(names).toContain('st_agents');
  });

  it('all input fields are optional (zero-arg form valid)', async () => {
    setupIdentity('alice');
    await boot();
    const r = await client.listTools();
    const tool = r.tools.find((t) => t.name === 'st_agents');
    expect(tool?.inputSchema?.required ?? []).toEqual([]);
  });
});

// ─── Happy paths ───────────────────────────────────────────────────────

describe('st_agents — happy paths', () => {
  it('no flags → returns all identities with MemberSummary shape', async () => {
    setupIdentity('alice');
    setupIdentity('bob');
    setupIdentity('carol');
    await boot();
    const r = await call();
    expect(r.isError).toBeUndefined();
    const members = (r.structuredContent?.members as MemberShape[]) ?? [];
    expect(members.map((m) => m.identity)).toEqual(['alice', 'bob', 'carol']);
    for (const m of members) {
      expect(m).toHaveProperty('identity');
      expect(m).toHaveProperty('status');
      expect(m).toHaveProperty('name');
      expect(m).not.toHaveProperty('lastActivity');
      expect(m).not.toHaveProperty('inbox');
    }
  });

  it('sorted alphabetically by identity', async () => {
    setupIdentity('zebra');
    setupIdentity('alice');
    setupIdentity('manny');
    await boot();
    const r = await call();
    const members = (r.structuredContent?.members as MemberShape[]) ?? [];
    expect(members.map((m) => m.identity)).toEqual(['alice', 'manny', 'zebra']);
  });

  it('status: "available" → only matching identities', async () => {
    setupIdentity('alice');
    setupIdentity('bob');
    setupIdentity('carol');
    setStatus('alice', 'available');
    setStatus('bob', 'busy');
    // carol leaves status unset → offline
    await boot();
    const r = await call({ status: 'available' });
    const members = (r.structuredContent?.members as MemberShape[]) ?? [];
    expect(members.map((m) => m.identity)).toEqual(['alice']);
    expect(members[0]?.status).toBe('available');
  });

  it('status: "offline" picks up identities with no status file', async () => {
    setupIdentity('alice');
    setupIdentity('bob');
    setStatus('alice', 'available');
    // bob has no status file → effective offline
    await boot();
    const r = await call({ status: 'offline' });
    const members = (r.structuredContent?.members as MemberShape[]) ?? [];
    expect(members.map((m) => m.identity)).toEqual(['bob']);
  });

  it('enrich: true → returns MemberSummaryEnriched with extra fields', async () => {
    setupIdentity('alice');
    setupIdentity('bob');
    // Drop a valid-grammar inbox file under bob so inbox count is 1.
    writeFileSync(
      join(coordRoot, 'bob', 'inbox', '1714826789010-aaaaaa.md'),
      '---\nfrom: alice\n---\nhi bob\n'
    );
    await boot();
    const r = await call({ enrich: true });
    const members = (r.structuredContent?.members as MemberShape[]) ?? [];
    expect(members).toHaveLength(2);
    for (const m of members) {
      expect(m).toHaveProperty('lastActivity');
      expect(m).toHaveProperty('inbox');
    }
    const bob = members.find((m) => m.identity === 'bob');
    expect(bob?.inbox).toBe(1);
    expect(typeof bob?.lastActivity).toBe('number');
  });

  it('empty $COORD_ROOT → returns []', async () => {
    // boot without setupIdentity — coordRoot has no identities at all.
    mkdirSync(coordRoot, { recursive: true });
    await boot();
    const r = await call();
    expect(r.isError).toBeUndefined();
    expect(r.structuredContent?.members).toEqual([]);
  });

  it('content[0].text pluralizes singular vs plural', async () => {
    setupIdentity('alice');
    await boot();
    const one = await call();
    expect(one.content?.[0]?.text).toBe('1 agent');
    setupIdentity('bob');
    setupIdentity('carol');
    const three = await call();
    expect(three.content?.[0]?.text).toBe('3 agents');
  });
});

// ─── Schema validation ─────────────────────────────────────────────────

describe('st_agents — schema validation', () => {
  it('invalid status value → tool errors (zod enum)', async () => {
    setupIdentity('alice');
    await boot();
    const r = await call({ status: 'urgent' });
    expect(r.isError).toBe(true);
  });

  it('accepts `away` as a valid filter value (brief-029)', async () => {
    setupIdentity('alice');
    setupIdentity('bob');
    setStatus('alice', 'away');
    setStatus('bob', 'busy');
    await boot();
    const r = await call({ status: 'away' });
    expect(r.isError).toBeUndefined();
    const members = (r.structuredContent?.members as Array<{
      identity: string;
    }>) ?? [];
    expect(members.map((m) => m.identity)).toEqual(['alice']);
  });

  it('enrich as a string → tool errors', async () => {
    setupIdentity('alice');
    await boot();
    const r = await call({ enrich: 'yes' });
    expect(r.isError).toBe(true);
  });
});
