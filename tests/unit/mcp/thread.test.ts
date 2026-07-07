// tests/unit/mcp/thread.test.ts — st_msg_thread tool, in-memory.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
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
  scratch = mkdtempSync(join(tmpdir(), 'coord-mcp-thread-'));
  stRoot = join(scratch, 'coord');
  for (const id of ['alice', 'bob']) {
    mkdirSync(join(stRoot, id, 'inbox'), { recursive: true });
    mkdirSync(join(stRoot, id, 'archive'), { recursive: true });
  }
  handle = createMcpServer({
    root: stRoot,
    identity: asIdentity('alice'),
  });
  client = new Client({ name: 'test-thread', version: '1.0' });
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
  return (await client.callTool({
    name: 'st_msg_thread',
    arguments: args,
  })) as CallResult;
}

interface PlantOpts {
  to: string;
  filename: string;
  from: string;
  subject?: string;
  inReplyTo?: string;
  folder?: 'inbox' | 'archive';
}

function plant(opts: PlantOpts): void {
  const folder = opts.folder ?? 'inbox';
  const dir = join(stRoot, opts.to, folder);
  mkdirSync(dir, { recursive: true });
  let head = `from: ${opts.from}\n`;
  if (opts.subject) head += `subject: ${opts.subject}\n`;
  if (opts.inReplyTo) head += `in-reply-to: ${opts.inReplyTo}\n`;
  writeFileSync(join(dir, opts.filename), `---\n${head}---\nbody\n`);
}

interface ThreadMessage {
  filename: string;
  identity: string;
  folder: string;
  message: { from: string; subject?: string; body: string };
}

function messages(r: CallResult): ThreadMessage[] {
  return (r.structuredContent?.messages ?? []) as ThreadMessage[];
}

// ─── Tools/list ────────────────────────────────────────────────────────

describe('st_msg_thread — tools/list registration', () => {
  it('registers with required filename + optional identity/tree', async () => {
    const r = await client.listTools();
    const tool = r.tools.find((t) => t.name === 'st_msg_thread');
    expect(tool).toBeDefined();
    expect(tool?.inputSchema?.required).toEqual(['filename']);
    expect(tool?.outputSchema).toBeDefined();
  });
});

// ─── Singleton ────────────────────────────────────────────────────────

describe('st_msg_thread — singleton', () => {
  it('one message, no in-reply-to → array of length 1', async () => {
    plant({
      to: 'alice',
      filename: '1714826789010-aaaaaa.md',
      from: 'bob',
      subject: 'solo',
    });
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    expect(r.isError).toBeUndefined();
    const msgs = messages(r);
    expect(msgs).toHaveLength(1);
    expect(msgs[0]?.filename).toBe('1714826789010-aaaaaa.md');
    expect(msgs[0]?.message.subject).toBe('solo');
  });

  it('content[0].text uses singular pluralization', async () => {
    plant({ to: 'alice', filename: '1714826789010-aaaaaa.md', from: 'bob' });
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    expect(r.content?.[0]?.text).toBe('1 message in thread');
  });
});

// ─── Linear chain ─────────────────────────────────────────────────────

describe('st_msg_thread — linear chain (flat default)', () => {
  it('walks ancestors and prints flat chronological', async () => {
    plant({
      to: 'alice',
      filename: '1714826789010-aaaaaa.md',
      from: 'bob',
      subject: 'root',
    });
    plant({
      to: 'alice',
      filename: '1714826789020-bbbbbb.md',
      from: 'bob',
      subject: 'child',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    plant({
      to: 'alice',
      filename: '1714826789030-cccccc.md',
      from: 'bob',
      subject: 'grandchild',
      inReplyTo: '1714826789020-bbbbbb.md',
    });
    const r = await call({ filename: '1714826789030-cccccc.md' });
    const msgs = messages(r);
    expect(msgs.map((m) => m.filename)).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789020-bbbbbb.md',
      '1714826789030-cccccc.md',
    ]);
    expect(r.content?.[0]?.text).toBe('3 messages in thread');
  });
});

// ─── Branching descendants ────────────────────────────────────────────

describe('st_msg_thread — branching descendants', () => {
  it('all descendants appear, sorted by filename', async () => {
    plant({ to: 'alice', filename: '1714826789010-aaaaaa.md', from: 'bob', subject: 'root' });
    plant({
      to: 'alice',
      filename: '1714826789020-bbbbbb.md',
      from: 'bob',
      subject: 'reply1',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    plant({
      to: 'alice',
      filename: '1714826789030-cccccc.md',
      from: 'carol',
      subject: 'reply2',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    plant({
      to: 'alice',
      filename: '1714826789040-dddddd.md',
      from: 'bob',
      subject: 'subreply',
      inReplyTo: '1714826789020-bbbbbb.md',
    });
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    expect(messages(r).map((m) => m.filename)).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789020-bbbbbb.md',
      '1714826789030-cccccc.md',
      '1714826789040-dddddd.md',
    ]);
  });
});

// ─── tree mode ────────────────────────────────────────────────────────

describe('st_msg_thread — tree mode', () => {
  it('tree=true preserves depth-indented hierarchy in walk order', async () => {
    plant({ to: 'alice', filename: '1714826789010-aaaaaa.md', from: 'bob', subject: 'root' });
    plant({
      to: 'alice',
      filename: '1714826789020-bbbbbb.md',
      from: 'bob',
      subject: 'reply1',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    plant({
      to: 'alice',
      filename: '1714826789030-cccccc.md',
      from: 'carol',
      subject: 'reply2',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    plant({
      to: 'alice',
      filename: '1714826789040-dddddd.md',
      from: 'bob',
      subject: 'subreply',
      inReplyTo: '1714826789020-bbbbbb.md',
    });
    const r = await call({ filename: '1714826789010-aaaaaa.md', tree: true });
    expect(messages(r).map((m) => m.filename)).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789020-bbbbbb.md',
      '1714826789040-dddddd.md',
      '1714826789030-cccccc.md',
    ]);
  });
});

// ─── Cross-identity walk ──────────────────────────────────────────────

describe('st_msg_thread — cross-identity walk', () => {
  it('reaches messages in other identities via in-reply-to', async () => {
    // alice → bob: lives in bob/inbox/
    plant({
      to: 'bob',
      filename: '1714826789010-aaaaaa.md',
      from: 'alice',
      subject: 'hi bob',
    });
    // bob → alice: lives in alice/inbox/
    plant({
      to: 'alice',
      filename: '1714826789020-bbbbbb.md',
      from: 'bob',
      subject: 're hi',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    // alice → bob, follow-up
    plant({
      to: 'bob',
      filename: '1714826789030-cccccc.md',
      from: 'alice',
      subject: 're re hi',
      inReplyTo: '1714826789020-bbbbbb.md',
    });
    const r = await call({
      filename: '1714826789020-bbbbbb.md',
      identity: 'alice',
    });
    expect(messages(r).map((m) => m.filename)).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789020-bbbbbb.md',
      '1714826789030-cccccc.md',
    ]);
  });
});

// ─── Inbox + archive ──────────────────────────────────────────────────

describe('st_msg_thread — spans inbox and archive', () => {
  it('finds an ancestor that has been archived', async () => {
    plant({
      to: 'alice',
      filename: '1714826789010-aaaaaa.md',
      from: 'bob',
      subject: 'root',
      folder: 'archive',
    });
    plant({
      to: 'alice',
      filename: '1714826789020-bbbbbb.md',
      from: 'bob',
      subject: 'child',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    const r = await call({ filename: '1714826789020-bbbbbb.md' });
    expect(messages(r).map((m) => m.filename)).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789020-bbbbbb.md',
    ]);
  });
});

// ─── Robustness ───────────────────────────────────────────────────────

describe('st_msg_thread — robustness', () => {
  it('orphan ancestor: only the seed walks', async () => {
    plant({
      to: 'alice',
      filename: '1714826789020-bbbbbb.md',
      from: 'bob',
      subject: 'child',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    const r = await call({ filename: '1714826789020-bbbbbb.md' });
    expect(messages(r).map((m) => m.filename)).toEqual([
      '1714826789020-bbbbbb.md',
    ]);
  });

  it('cycle in in-reply-to terminates and yields unique messages', async () => {
    plant({
      to: 'alice',
      filename: '1714826789010-aaaaaa.md',
      from: 'bob',
      subject: 'X',
      inReplyTo: '1714826789020-bbbbbb.md',
    });
    plant({
      to: 'alice',
      filename: '1714826789020-bbbbbb.md',
      from: 'bob',
      subject: 'Y',
      inReplyTo: '1714826789010-aaaaaa.md',
    });
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    const filenames = messages(r).map((m) => m.filename).sort();
    expect(filenames).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789020-bbbbbb.md',
    ]);
  });
});

// ─── Schema validation ────────────────────────────────────────────────

describe('st_msg_thread — schema validation', () => {
  it('rejects missing filename', async () => {
    const r = await call({});
    expect(r.isError).toBe(true);
  });

  it('rejects filename of wrong type', async () => {
    const r = await call({ filename: 42 });
    expect(r.isError).toBe(true);
  });

  it('rejects tree of wrong type (string)', async () => {
    plant({ to: 'alice', filename: '1714826789010-aaaaaa.md', from: 'bob' });
    const r = await call({
      filename: '1714826789010-aaaaaa.md',
      tree: 'yes',
    });
    expect(r.isError).toBe(true);
  });
});

// ─── Typed error mapping ──────────────────────────────────────────────

describe('st_msg_thread — typed error mapping', () => {
  it('seed not found → MESSAGE_NOT_FOUND', async () => {
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    expect(errorCode(r)).toBe('MESSAGE_NOT_FOUND');
  });

  it('invalid filename grammar → INVALID_FILENAME', async () => {
    const r = await call({ filename: 'garbage' });
    expect(errorCode(r)).toBe('INVALID_FILENAME');
  });

  it('legacy 3-segment filename → INVALID_FILENAME', async () => {
    const r = await call({ filename: '1714826789010-operator-aaaaaa.md' });
    expect(errorCode(r)).toBe('INVALID_FILENAME');
  });

  it('unknown identity → IDENTITY_NOT_HOSTED', async () => {
    const r = await call({
      filename: '1714826789010-aaaaaa.md',
      identity: 'ghost',
    });
    expect(errorCode(r)).toBe('IDENTITY_NOT_HOSTED');
  });

  it('invalid identity grammar → INVALID_IDENTITY', async () => {
    const r = await call({
      filename: '1714826789010-aaaaaa.md',
      identity: 'INVALID',
    });
    expect(errorCode(r)).toBe('INVALID_IDENTITY');
  });
});

// ─── Output-shape regression ──────────────────────────────────────────

describe('st_msg_thread — output shape', () => {
  it('every message carries identity, folder, filename + parsed message', async () => {
    plant({ to: 'alice', filename: '1714826789010-aaaaaa.md', from: 'bob' });
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    const msgs = messages(r);
    expect(msgs[0]).toMatchObject({
      filename: '1714826789010-aaaaaa.md',
      identity: 'alice',
      folder: 'inbox',
      message: { from: 'bob', body: 'body\n' },
    });
  });

  it('omits message optional keys when absent on disk', async () => {
    plant({ to: 'alice', filename: '1714826789010-aaaaaa.md', from: 'bob' });
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    const m = messages(r)[0]?.message as Record<string, unknown>;
    expect('subject' in m).toBe(false);
    expect('inReplyTo' in m).toBe(false);
  });
});
