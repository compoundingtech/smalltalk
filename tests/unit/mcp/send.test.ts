// tests/unit/mcp/send.test.ts — st_msg_send tool, in-memory.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import { CallToolResultSchema } from '@modelcontextprotocol/sdk/types.js';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { createMcpServer } from '../../../src/mcp/index.ts';
import { errorCode, errorPayload } from "./_helpers.ts";
import { asIdentity } from '../../../src/types.ts';

let scratch: string;
let stRoot: string;
let client: Client;
let handle: ReturnType<typeof createMcpServer>;

beforeEach(async () => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-mcp-send-'));
  stRoot = join(scratch, 'coord');
  mkdirSync(join(stRoot, 'alice', 'inbox'), { recursive: true });
  mkdirSync(join(stRoot, 'alice', 'archive'), { recursive: true });
  mkdirSync(join(stRoot, 'bob', 'inbox'), { recursive: true });
  mkdirSync(join(stRoot, 'bob', 'archive'), { recursive: true });
  handle = createMcpServer({
    root: stRoot,
    identity: asIdentity('alice'),
  });
  client = new Client({ name: 'test-send', version: '1.0' });
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

async function call(args: Record<string, unknown>): Promise<CallResult> {
  const r = await client.callTool({ name: 'st_msg_send', arguments: args });
  return r as CallResult;
}

// ─── tools/list shape ──────────────────────────────────────────────────

describe('st_msg_send — tools/list registration', () => {
  it('appears in tools/list with input + output schemas', async () => {
    const r = await client.listTools();
    const tool = r.tools.find((t) => t.name === 'st_msg_send');
    expect(tool).toBeDefined();
    expect(tool?.description).toMatch(/inbox/i);
    expect(tool?.inputSchema).toBeDefined();
    expect(tool?.outputSchema).toBeDefined();
  });

  it('inputSchema declares to + body as required and the rest optional', async () => {
    const r = await client.listTools();
    const tool = r.tools.find((t) => t.name === 'st_msg_send');
    expect(tool?.inputSchema?.required).toEqual(
      expect.arrayContaining(['to', 'body'])
    );
    expect(tool?.inputSchema?.required).not.toContain('from');
    expect(tool?.inputSchema?.required).not.toContain('subject');
  });
});

// ─── Happy paths ───────────────────────────────────────────────────────

describe('st_msg_send — happy paths', () => {
  it('minimal: to + body → file written + structuredContent.filename', async () => {
    const r = await call({ to: 'bob', body: 'hello bob' });
    expect(r.isError).toBeUndefined();
    const filename = r.structuredContent?.filename as string;
    expect(filename).toMatch(/^[0-9]{13}-[0-9a-z]{6}\.md$/);
    expect(r.structuredContent?.identity).toBe('bob');
    expect(existsSync(join(stRoot, 'bob', 'inbox', filename))).toBe(true);
  });

  it('full options: subject + tags + priority + inReplyTo + from', async () => {
    const r = await call({
      to: 'bob',
      body: 'reply',
      from: 'alice',
      subject: 're: hi',
      inReplyTo: '1714826789012-abcdef.md',
      tags: ['auth', 'coordination'],
      priority: 'high',
    });
    expect(r.isError).toBeUndefined();
    const filename = r.structuredContent?.filename as string;
    const text = readFileSync(
      join(stRoot, 'bob', 'inbox', filename),
      'utf8'
    );
    expect(text).toContain('from: alice');
    expect(text).toContain('subject: "re: hi"');
    expect(text).toContain('in-reply-to: 1714826789012-abcdef.md');
    expect(text).toContain('tags: [auth, coordination]');
    expect(text).toContain('priority: high');
  });

  it('default from is the Coord identity (alice)', async () => {
    const r = await call({ to: 'bob', body: 'msg' });
    const filename = r.structuredContent?.filename as string;
    const text = readFileSync(
      join(stRoot, 'bob', 'inbox', filename),
      'utf8'
    );
    expect(text).toContain('from: alice');
  });

  it('opts.from override changes the sender', async () => {
    // bob has folders too; sending --from bob is allowed when bob's
    // identity folder exists locally.
    const r = await call({ to: 'alice', body: 'm', from: 'bob' });
    const filename = r.structuredContent?.filename as string;
    const text = readFileSync(
      join(stRoot, 'alice', 'inbox', filename),
      'utf8'
    );
    expect(text).toContain('from: bob');
  });

  it('content[0].text is a human-readable summary', async () => {
    const r = await call({ to: 'bob', body: 'hi' });
    expect(r.content?.[0]?.text).toMatch(/^sent: bob\//);
  });

  it('result is a valid CallToolResult schema', async () => {
    const raw = await client.callTool({
      name: 'st_msg_send',
      arguments: { to: 'bob', body: 'hi' },
    });
    expect(() => CallToolResultSchema.parse(raw)).not.toThrow();
  });
});

// ─── Schema validation ─────────────────────────────────────────────────

describe('st_msg_send — schema validation', () => {
  it('rejects missing to', async () => {
    const r = await call({ body: 'no recipient' });
    expect(r.isError).toBe(true);
  });

  it('rejects missing body', async () => {
    const r = await call({ to: 'bob' });
    expect(r.isError).toBe(true);
  });

  it('rejects tags as a string (must be array)', async () => {
    const r = await call({ to: 'bob', body: 'm', tags: 'a,b' });
    expect(r.isError).toBe(true);
  });

  it('rejects invalid priority enum value', async () => {
    const r = await call({ to: 'bob', body: 'm', priority: 'urgent' });
    expect(r.isError).toBe(true);
  });

  it('rejects body of the wrong type (number)', async () => {
    const r = await call({ to: 'bob', body: 42 });
    expect(r.isError).toBe(true);
  });
});

// ─── Error mapping ─────────────────────────────────────────────────────

describe('st_msg_send — typed error mapping', () => {
  it('invalid recipient name → INVALID_IDENTITY', async () => {
    const r = await call({ to: 'INVALID', body: 'm' });
    expect(r.isError).toBe(true);
    expect(errorCode(r)).toBe('INVALID_IDENTITY');
    expect(r.content?.[0]?.text).toMatch(/^INVALID_IDENTITY:/);
  });

  it('reserved recipient name → INVALID_IDENTITY', async () => {
    const r = await call({ to: 'inbox', body: 'm' });
    expect(errorCode(r)).toBe('INVALID_IDENTITY');
  });

  it('--from points at a non-hosted identity → IDENTITY_NOT_HOSTED', async () => {
    const r = await call({ to: 'bob', body: 'm', from: 'ghost' });
    expect(errorCode(r)).toBe('IDENTITY_NOT_HOSTED');
    expect(errorPayload(r)?.details).toEqual({ identity: 'ghost' });
  });

  it('--from with invalid identity grammar → INVALID_IDENTITY', async () => {
    const r = await call({ to: 'bob', body: 'm', from: 'INVALID' });
    expect(errorCode(r)).toBe('INVALID_IDENTITY');
  });

  it('empty body → EMPTY_BODY', async () => {
    const r = await call({ to: 'bob', body: '' });
    expect(errorCode(r)).toBe('EMPTY_BODY');
    expect(r.content?.[0]?.text).toMatch(/^EMPTY_BODY:/);
  });

  it('invalid in-reply-to filename → INVALID_FILENAME', async () => {
    const r = await call({ to: 'bob', body: 'm', inReplyTo: 'garbage' });
    expect(errorCode(r)).toBe('INVALID_FILENAME');
  });

  it('legacy 3-segment in-reply-to → INVALID_FILENAME', async () => {
    const r = await call({
      to: 'bob',
      body: 'm',
      inReplyTo: '1714826789012-operator-abcdef.md',
    });
    expect(errorCode(r)).toBe('INVALID_FILENAME');
  });

  it('every error response carries content + _meta["coord/error"] + isError', async () => {
    const r = await call({ to: 'INVALID', body: 'm' });
    expect(r.isError).toBe(true);
    expect(r.content?.[0]?.text).toMatch(/^INVALID_IDENTITY:/);
    expect(errorPayload(r)).toMatchObject({
      code: 'INVALID_IDENTITY',
      message: expect.stringContaining('INVALID'),
    });
    expect(r.structuredContent).toBeUndefined();
  });
});

// ─── Concurrency + uniqueness ─────────────────────────────────────────

describe('st_msg_send — concurrency', () => {
  it('10 parallel sends produce 10 distinct files', async () => {
    const calls = Array.from({ length: 10 }, (_, i) =>
      call({ to: 'bob', body: `msg-${i}` })
    );
    const results = await Promise.all(calls);
    const filenames = results
      .map((r) => r.structuredContent?.filename as string)
      .filter(Boolean);
    expect(filenames).toHaveLength(10);
    expect(new Set(filenames).size).toBe(10);
  });
});

// ─── Content + edge cases ─────────────────────────────────────────────

describe('st_msg_send — content + edge cases', () => {
  it('subject with embedded colons survives the YAML quoting roundtrip', async () => {
    const r = await call({
      to: 'bob',
      body: 'm',
      subject: 'auth: drop legacy cookie',
    });
    const f = r.structuredContent?.filename as string;
    const text = readFileSync(join(stRoot, 'bob', 'inbox', f), 'utf8');
    expect(text).toContain('subject: "auth: drop legacy cookie"');
  });

  it('subject with embedded newlines is escaped', async () => {
    const r = await call({
      to: 'bob',
      body: 'm',
      subject: 'line1\nline2',
    });
    const f = r.structuredContent?.filename as string;
    const text = readFileSync(join(stRoot, 'bob', 'inbox', f), 'utf8');
    expect(text).toContain('subject: "line1\\nline2"');
  });

  it('1MB body survives byte-for-byte', async () => {
    const body = 'a'.repeat(1024 * 1024);
    const r = await call({ to: 'bob', body });
    expect(r.isError).toBeUndefined();
    const f = r.structuredContent?.filename as string;
    const text = readFileSync(join(stRoot, 'bob', 'inbox', f), 'utf8');
    expect(text.endsWith(`${body}\n`)).toBe(true);
  });

  it('tags get YAML-quoted in the inline list', async () => {
    const r = await call({
      to: 'bob',
      body: 'm',
      tags: ['has space', 'simple'],
    });
    const f = r.structuredContent?.filename as string;
    const text = readFileSync(join(stRoot, 'bob', 'inbox', f), 'utf8');
    // "has space" needs quoting; "simple" stays plain — mixed flow list.
    expect(text).toContain('tags: ["has space", simple]');
  });
});
