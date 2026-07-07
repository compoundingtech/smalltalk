// tests/unit/mcp/resource.test.ts — st_resource_{add,ls,read,remove}
// MCP tools driven over an in-memory transport. Same pattern as the
// other mcp/* tool tests.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import { mkdirSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { createMcpServer } from '../../../src/mcp/index.ts';
import { asIdentity } from '../../../src/types.ts';

let scratch: string;
let stRoot: string;
let client: Client;
let handle: ReturnType<typeof createMcpServer>;

function setupIdentity(id: string): void {
  mkdirSync(join(stRoot, id, 'inbox'), { recursive: true });
  mkdirSync(join(stRoot, id, 'archive'), { recursive: true });
}

async function boot(identity = 'alice'): Promise<void> {
  handle = createMcpServer({
    root: stRoot,
    identity: asIdentity(identity),
  });
  client = new Client({ name: 'test-resource', version: '1.0' });
  const [c, s] = InMemoryTransport.createLinkedPair();
  await Promise.all([client.connect(c), handle.mcp.connect(s)]);
}

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-mcp-resource-'));
  stRoot = join(scratch, 'coord');
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

async function call(
  name: string,
  args: Record<string, unknown> = {}
): Promise<CallResult> {
  return (await client.callTool({ name, arguments: args })) as CallResult;
}

// ─── Registration ──────────────────────────────────────────────────────

describe('st_resource_* — registration', () => {
  it('all four tools appear in tools/list with input + output schemas', async () => {
    setupIdentity('alice');
    await boot();
    const r = await client.listTools();
    const names = r.tools.map((t) => t.name);
    for (const n of [
      'st_resource_add',
      'st_resource_ls',
      'st_resource_read',
      'st_resource_remove',
      'st_resource_add',
      'st_resource_ls',
      'st_resource_read',
      'st_resource_remove',
    ]) {
      expect(names).toContain(n);
    }
    const add = r.tools.find((t) => t.name === 'st_resource_add')!;
    expect(add.inputSchema).toBeDefined();
    expect(add.outputSchema).toBeDefined();
  });
});

// ─── Add ───────────────────────────────────────────────────────────────

describe('st_resource_add', () => {
  it('writes a resource under the agent\'s own identity', async () => {
    setupIdentity('alice');
    await boot();
    const r = await call('st_resource_add', {
      url: 'https://example.com/foo',
      title: 'foo',
      tags: ['x', 'y'],
      body: 'hello',
    });
    expect(r.isError).toBeUndefined();
    const sc = r.structuredContent as {
      filename: string;
      identity: string;
    };
    expect(sc.identity).toBe('alice');
    expect(/^[0-9]{13}-[0-9a-z]{6}\.md$/.test(sc.filename)).toBe(true);
  });

  it('rejects an unscheme\'d URL with an error result', async () => {
    setupIdentity('alice');
    await boot();
    const r = await call('st_resource_add', { url: 'example.com' });
    expect(r.isError).toBe(true);
  });

  it('accepts pty:// scheme', async () => {
    setupIdentity('alice');
    await boot();
    const r = await call('st_resource_add', {
      url: 'pty://my-session',
    });
    expect(r.isError).toBeUndefined();
  });

  it('persists the optional relation field when set', async () => {
    setupIdentity('alice');
    await boot();
    const added = (await call('st_resource_add', {
      url: 'https://example.com',
      relation: 'owns',
    })) as CallResult;
    const filename = (added.structuredContent as { filename: string })
      .filename;
    const r = (await call('st_resource_read', { filename })) as CallResult;
    const sc = r.structuredContent as { relation: string | null };
    expect(sc.relation).toBe('owns');
  });

  it('omits relation by default → null in read output', async () => {
    setupIdentity('alice');
    await boot();
    const added = (await call('st_resource_add', {
      url: 'https://example.com',
    })) as CallResult;
    const filename = (added.structuredContent as { filename: string })
      .filename;
    const r = (await call('st_resource_read', { filename })) as CallResult;
    const sc = r.structuredContent as { relation: string | null };
    expect(sc.relation).toBeNull();
  });
});

// ─── Ls ────────────────────────────────────────────────────────────────

describe('st_resource_ls', () => {
  it('lists own resources after a few adds', async () => {
    setupIdentity('alice');
    await boot();
    await call('st_resource_add', { url: 'https://example.com/a' });
    await call('st_resource_add', { url: 'https://example.com/b' });
    const r = await call('st_resource_ls', {});
    expect(r.isError).toBeUndefined();
    const sc = r.structuredContent as {
      identity: string;
      resources: Array<{ filename: string; url: string }>;
    };
    expect(sc.identity).toBe('alice');
    expect(sc.resources).toHaveLength(2);
    const urls = sc.resources.map((it) => it.url).sort();
    expect(urls).toEqual(['https://example.com/a', 'https://example.com/b']);
  });

  it('lists a peer\'s resources when identity passed explicitly', async () => {
    setupIdentity('alice');
    setupIdentity('bob');
    await boot('alice');
    // Alice can't add to bob's resources via the MCP tool (it always
    // writes to coord.identity). Plant directly.
    mkdirSync(join(stRoot, 'bob', 'resources'), { recursive: true });
    const bobFile = join(
      stRoot,
      'bob',
      'resources',
      '1714826789010-aaaaaa.md'
    );
    const { writeFileSync } = await import('node:fs');
    writeFileSync(bobFile, '---\nurl: https://bob.example/\n---\nbody\n');
    const r = await call('st_resource_ls', { identity: 'bob' });
    expect(r.isError).toBeUndefined();
    const sc = r.structuredContent as {
      identity: string;
      resources: Array<{ filename: string; url: string }>;
    };
    expect(sc.identity).toBe('bob');
    expect(sc.resources).toHaveLength(1);
    expect(sc.resources[0]?.url).toBe('https://bob.example/');
  });

  it('empty resources/ → empty array', async () => {
    setupIdentity('alice');
    await boot();
    const r = await call('st_resource_ls', {});
    const sc = r.structuredContent as { resources: unknown[] };
    expect(sc.resources).toEqual([]);
  });
});

// ─── Read ──────────────────────────────────────────────────────────────

describe('st_resource_read', () => {
  it('round-trips url + title + tags + body', async () => {
    setupIdentity('alice');
    await boot();
    const added = (await call('st_resource_add', {
      url: 'https://example.com',
      title: 'eg',
      tags: ['a'],
      body: 'desc',
    })) as CallResult;
    const filename = (added.structuredContent as { filename: string })
      .filename;
    const r = await call('st_resource_read', { filename });
    expect(r.isError).toBeUndefined();
    const sc = r.structuredContent as {
      identity: string;
      filename: string;
      url: string;
      title: string | null;
      tags: string[];
      body: string;
    };
    expect(sc.identity).toBe('alice');
    expect(sc.filename).toBe(filename);
    expect(sc.url).toBe('https://example.com');
    expect(sc.title).toBe('eg');
    expect(sc.tags).toEqual(['a']);
    expect(sc.body).toContain('desc');
  });

  it('errors with RESOURCE_NOT_FOUND when filename missing', async () => {
    setupIdentity('alice');
    await boot();
    const r = await call('st_resource_read', {
      filename: '1714826789010-aaaaaa.md',
    });
    expect(r.isError).toBe(true);
  });
});

// ─── Remove ────────────────────────────────────────────────────────────

describe('st_resource_remove', () => {
  it('removes a previously-added resource', async () => {
    setupIdentity('alice');
    await boot();
    const added = (await call('st_resource_add', {
      url: 'https://example.com',
    })) as CallResult;
    const filename = (added.structuredContent as { filename: string })
      .filename;
    const r = await call('st_resource_remove', { filename });
    expect(r.isError).toBeUndefined();
    const sc = r.structuredContent as {
      identity: string;
      filename: string;
      removed: boolean;
    };
    expect(sc.removed).toBe(true);
    expect(sc.identity).toBe('alice');
    // And it's gone.
    const ls = await call('st_resource_ls', {});
    const lsSc = ls.structuredContent as { resources: unknown[] };
    expect(lsSc.resources).toHaveLength(0);
  });

  it('errors when the filename is unknown', async () => {
    setupIdentity('alice');
    await boot();
    const r = await call('st_resource_remove', {
      filename: '1714826789010-aaaaaa.md',
    });
    expect(r.isError).toBe(true);
  });
});

// ─── st_ alias parity ─────────────────────────────────────────────────

describe('st_resource_* (dual-prefix parity)', () => {
  it('st_resource_add reaches the same handler as st_resource_add', async () => {
    setupIdentity('alice');
    await boot();
    const r = await call('st_resource_add', {
      url: 'https://example.com',
    });
    expect(r.isError).toBeUndefined();
    const ls = await call('st_resource_ls', {});
    const sc = ls.structuredContent as { resources: unknown[] };
    expect(sc.resources).toHaveLength(1);
  });
});
