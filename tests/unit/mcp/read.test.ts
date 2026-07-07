// tests/unit/mcp/read.test.ts — st_msg_read tool, in-memory.

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
  scratch = mkdtempSync(join(tmpdir(), 'coord-mcp-read-'));
  stRoot = join(scratch, 'coord');
  for (const id of ['alice', 'bob']) {
    mkdirSync(join(stRoot, id, 'inbox'), { recursive: true });
    mkdirSync(join(stRoot, id, 'archive'), { recursive: true });
  }
  handle = createMcpServer({
    root: stRoot,
    identity: asIdentity('alice'),
  });
  client = new Client({ name: 'test-read', version: '1.0' });
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
    name: 'st_msg_read',
    arguments: args,
  })) as CallResult;
}

function writeFm(
  identity: string,
  filename: string,
  fm: Record<string, string>,
  body: string,
  folder: 'inbox' | 'archive' = 'inbox'
): void {
  const head = Object.entries(fm)
    .map(([k, v]) => `${k}: ${v}`)
    .join('\n');
  writeFileSync(
    join(stRoot, identity, folder, filename),
    `---\n${head}\n---\n${body}\n`
  );
}

// ─── tools/list ────────────────────────────────────────────────────────

describe('st_msg_read — tools/list registration', () => {
  it('registers with required filename and optional identity/fromArchive', async () => {
    const r = await client.listTools();
    const tool = r.tools.find((t) => t.name === 'st_msg_read');
    expect(tool).toBeDefined();
    expect(tool?.inputSchema?.required).toEqual(['filename']);
    expect(tool?.outputSchema).toBeDefined();
  });
});

// ─── Happy paths ───────────────────────────────────────────────────────

describe('st_msg_read — happy paths', () => {
  it('returns parsed Message + location for an inbox file', async () => {
    writeFm(
      'alice',
      '1714826789010-aaaaaa.md',
      { from: 'bob', subject: 'hi' },
      'the body'
    );
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    expect(r.isError).toBeUndefined();
    expect(r.structuredContent).toMatchObject({
      filename: '1714826789010-aaaaaa.md',
      identity: 'alice',
      folder: 'inbox',
      message: { from: 'bob', subject: 'hi', body: 'the body\n' },
    });
  });

  it('content[0].text is "<folder>/<identity>/<filename>"', async () => {
    writeFm(
      'alice',
      '1714826789010-aaaaaa.md',
      { from: 'bob' },
      'b'
    );
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    expect(r.content?.[0]?.text).toBe(
      'inbox/alice/1714826789010-aaaaaa.md'
    );
  });

  it('explicit identity overrides default', async () => {
    writeFm('bob', '1714826789010-aaaaaa.md', { from: 'alice' }, 'b');
    const r = await call({
      filename: '1714826789010-aaaaaa.md',
      identity: 'bob',
    });
    expect(r.structuredContent?.identity).toBe('bob');
  });

  it('fromArchive=true reads the archive copy', async () => {
    writeFm(
      'alice',
      '1714826789010-aaaaaa.md',
      { from: 'bob' },
      'archived',
      'archive'
    );
    const r = await call({
      filename: '1714826789010-aaaaaa.md',
      fromArchive: true,
    });
    expect(r.structuredContent?.folder).toBe('archive');
    expect(
      (r.structuredContent?.message as { body: string }).body
    ).toBe('archived\n');
  });

  it('auto-fallback: not in inbox, IS in archive → reads archive', async () => {
    writeFm(
      'alice',
      '1714826789010-aaaaaa.md',
      { from: 'bob' },
      'archived',
      'archive'
    );
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    expect(r.structuredContent?.folder).toBe('archive');
  });

  it('inbox preferred when fromArchive is false and both exist', async () => {
    writeFm('alice', '1714826789010-aaaaaa.md', { from: 'bob' }, 'inbox-version', 'inbox');
    writeFm('alice', '1714826789010-aaaaaa.md', { from: 'bob' }, 'archive-version', 'archive');
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    expect(r.structuredContent?.folder).toBe('inbox');
    expect(
      (r.structuredContent?.message as { body: string }).body
    ).toBe('inbox-version\n');
  });

  it('fromArchive=true prefers archive even when inbox copy exists', async () => {
    writeFm('alice', '1714826789010-aaaaaa.md', { from: 'bob' }, 'inbox-v', 'inbox');
    writeFm('alice', '1714826789010-aaaaaa.md', { from: 'bob' }, 'arch-v', 'archive');
    const r = await call({
      filename: '1714826789010-aaaaaa.md',
      fromArchive: true,
    });
    expect(
      (r.structuredContent?.message as { body: string }).body
    ).toBe('arch-v\n');
  });
});

// ─── Optional frontmatter fields ──────────────────────────────────────

describe('st_msg_read — optional message fields', () => {
  it('omits optional keys from message when absent', async () => {
    writeFm('alice', '1714826789010-aaaaaa.md', { from: 'bob' }, 'b');
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    const m = r.structuredContent?.message as Record<string, unknown>;
    expect(m.from).toBe('bob');
    expect(m.body).toBe('b\n');
    expect('subject' in m).toBe(false);
    expect('inReplyTo' in m).toBe(false);
    expect('tags' in m).toBe(false);
    expect('priority' in m).toBe(false);
  });

  it('populates in-reply-to when present', async () => {
    writeFm(
      'alice',
      '1714826789010-aaaaaa.md',
      { from: 'bob', 'in-reply-to': '1714826789000-zzzzzz.md' },
      'b'
    );
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    expect(
      (r.structuredContent?.message as Record<string, string>).inReplyTo
    ).toBe('1714826789000-zzzzzz.md');
  });

  it('populates priority when present', async () => {
    writeFm(
      'alice',
      '1714826789010-aaaaaa.md',
      { from: 'bob', priority: 'high' },
      'b'
    );
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    expect(
      (r.structuredContent?.message as Record<string, string>).priority
    ).toBe('high');
  });
});

// ─── Schema validation ─────────────────────────────────────────────────

describe('st_msg_read — schema validation', () => {
  it('rejects missing filename', async () => {
    const r = await call({});
    expect(r.isError).toBe(true);
  });

  it('rejects filename of wrong type (number)', async () => {
    const r = await call({ filename: 42 });
    expect(r.isError).toBe(true);
  });

  it('rejects fromArchive of wrong type (string)', async () => {
    writeFm('alice', '1714826789010-aaaaaa.md', { from: 'bob' }, 'b');
    const r = await call({
      filename: '1714826789010-aaaaaa.md',
      fromArchive: 'yes',
    });
    expect(r.isError).toBe(true);
  });
});

// ─── Typed error mapping ──────────────────────────────────────────────

describe('st_msg_read — typed error mapping', () => {
  it('invalid filename grammar → INVALID_FILENAME', async () => {
    const r = await call({ filename: 'garbage' });
    expect(errorCode(r)).toBe('INVALID_FILENAME');
  });

  it('legacy 3-segment filename is accepted as outside .md → MESSAGE_NOT_FOUND when absent (task #128)', async () => {
    // Post task #128: a legacy 3-segment `.md` (or any off-format
    // `.md`) is valid to *reference* — the "outside message" path.
    // When the file doesn't exist, error is MESSAGE_NOT_FOUND.
    const r = await call({ filename: '1714826789010-operator-aaaaaa.md' });
    expect(errorCode(r)).toBe('MESSAGE_NOT_FOUND');
  });

  it('not found in either folder → MESSAGE_NOT_FOUND', async () => {
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    expect(errorCode(r)).toBe('MESSAGE_NOT_FOUND');
    expect(errorPayload(r)?.details).toMatchObject({
      filename: '1714826789010-aaaaaa.md',
    });
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

// ─── Edge cases ────────────────────────────────────────────────────────

describe('st_msg_read — edge cases', () => {
  it('file without frontmatter: message.from is empty, body is whole text', async () => {
    writeFileSync(
      join(stRoot, 'alice', 'inbox', '1714826789010-aaaaaa.md'),
      'just text\n'
    );
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    const m = r.structuredContent?.message as Record<string, string>;
    expect(m.from).toBe('');
    expect(m.body).toBe('just text\n');
  });

  it('empty file → empty body, untyped', async () => {
    writeFileSync(
      join(stRoot, 'alice', 'inbox', '1714826789010-aaaaaa.md'),
      ''
    );
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    const m = r.structuredContent?.message as Record<string, string>;
    expect(m.body).toBe('');
  });

  it('subject with embedded colon and newline survives parse roundtrip', async () => {
    // A message written via st_msg_send → st_msg_read roundtrip.
    const send = (await client.callTool({
      name: 'st_msg_send',
      arguments: {
        to: 'bob',
        body: 'm',
        subject: 'has: colon\nand newline',
      },
    })) as CallResult;
    const filename = send.structuredContent?.filename as string;
    const r = await call({ filename, identity: 'bob' });
    expect(
      (r.structuredContent?.message as Record<string, string>).subject
    ).toBe('has: colon\nand newline');
  });
});
