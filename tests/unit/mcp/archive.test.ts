// tests/unit/mcp/archive.test.ts — st_msg_archive tool, in-memory.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { createMcpServer } from '../../../src/mcp/index.ts';
import { errorCode, errorPayload } from "./_helpers.ts";
import { asIdentity } from '../../../src/types.ts';

let scratch: string;
let coordRoot: string;
let client: Client;
let handle: ReturnType<typeof createMcpServer>;

beforeEach(async () => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-mcp-archive-'));
  coordRoot = join(scratch, 'coord');
  for (const id of ['alice', 'bob']) {
    mkdirSync(join(coordRoot, id, 'inbox'), { recursive: true });
    mkdirSync(join(coordRoot, id, 'archive'), { recursive: true });
  }
  handle = createMcpServer({
    root: coordRoot,
    identity: asIdentity('alice'),
  });
  client = new Client({ name: 'test-archive', version: '1.0' });
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
    name: 'st_msg_archive',
    arguments: args,
  })) as CallResult;
}

function plant(
  identity: string,
  filename: string,
  body: string,
  folder: 'inbox' | 'archive' = 'inbox'
): void {
  writeFileSync(
    join(coordRoot, identity, folder, filename),
    `---\nfrom: bob\n---\n${body}\n`
  );
}

// ─── Tools/list ────────────────────────────────────────────────────────

describe('st_msg_archive — tools/list registration', () => {
  it('registers with required filename', async () => {
    const r = await client.listTools();
    const tool = r.tools.find((t) => t.name === 'st_msg_archive');
    expect(tool).toBeDefined();
    expect(tool?.inputSchema?.required).toEqual(['filename']);
    expect(tool?.outputSchema).toBeDefined();
  });
});

// ─── Happy paths ───────────────────────────────────────────────────────

describe('st_msg_archive — happy paths', () => {
  it('case 4 (clean rename): inbox file → archive', async () => {
    plant('alice', '1714826789010-aaaaaa.md', 'body');
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    expect(r.isError).toBeUndefined();
    expect(r.structuredContent).toMatchObject({
      filename: '1714826789010-aaaaaa.md',
      identity: 'alice',
      outcome: 'moved',
    });
    expect(
      existsSync(join(coordRoot, 'alice', 'inbox', '1714826789010-aaaaaa.md'))
    ).toBe(false);
    expect(
      existsSync(join(coordRoot, 'alice', 'archive', '1714826789010-aaaaaa.md'))
    ).toBe(true);
  });

  it('case 0 (post-sweep idempotent): inbox empty + archive present → outcome=idempotent', async () => {
    plant('alice', '1714826789010-aaaaaa.md', 'archived', 'archive');
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    expect(r.isError).toBeUndefined();
    expect(r.structuredContent?.outcome).toBe('idempotent');
  });

  it('case 2 (byte-identical twin): pre-command sweep removes inbox dup → outcome=idempotent', async () => {
    // The universal pre-command sweep (which fires before every coord
    // method) removes the byte-identical inbox copy first; archive then
    // sees inbox-empty + archive-present (case 0).
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(join(coordRoot, 'alice', 'inbox', f), 'same');
    writeFileSync(join(coordRoot, 'alice', 'archive', f), 'same');
    const r = await call({ filename: f });
    expect(r.structuredContent?.outcome).toBe('idempotent');
    expect(existsSync(join(coordRoot, 'alice', 'inbox', f))).toBe(false);
    expect(readFileSync(join(coordRoot, 'alice', 'archive', f), 'utf8')).toBe(
      'same'
    );
  });

  it('explicit identity override targets a different folder', async () => {
    plant('bob', '1714826789010-aaaaaa.md', 'b');
    const r = await call({
      filename: '1714826789010-aaaaaa.md',
      identity: 'bob',
    });
    expect(r.structuredContent?.identity).toBe('bob');
    expect(
      existsSync(join(coordRoot, 'bob', 'archive', '1714826789010-aaaaaa.md'))
    ).toBe(true);
  });

  it('content[0].text distinguishes moved vs idempotent', async () => {
    plant('alice', '1714826789010-aaaaaa.md', 'body');
    const moved = await call({ filename: '1714826789010-aaaaaa.md' });
    expect(moved.content?.[0]?.text).toBe(
      'archived: alice/1714826789010-aaaaaa.md'
    );

    plant('alice', '1714826789020-bbbbbb.md', 'b', 'archive');
    const idempotent = await call({ filename: '1714826789020-bbbbbb.md' });
    expect(idempotent.content?.[0]?.text).toBe(
      'archived (idempotent): alice/1714826789020-bbbbbb.md'
    );
  });
});

// ─── Schema validation ─────────────────────────────────────────────────

describe('st_msg_archive — schema validation', () => {
  it('rejects missing filename', async () => {
    const r = await call({});
    expect(r.isError).toBe(true);
  });

  it('rejects filename of wrong type (number)', async () => {
    const r = await call({ filename: 42 });
    expect(r.isError).toBe(true);
  });

  it('rejects identity of wrong type (number)', async () => {
    const r = await call({
      filename: '1714826789010-aaaaaa.md',
      identity: 42,
    });
    expect(r.isError).toBe(true);
  });
});

// ─── Typed error mapping ─────────────────────────────────────────────

describe('st_msg_archive — typed error mapping', () => {
  it('case 1 (not in either folder) → MESSAGE_NOT_FOUND', async () => {
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    expect(errorCode(r)).toBe('MESSAGE_NOT_FOUND');
  });

  it('case 3 (divergent twin) → ARCHIVE_CONFLICT', async () => {
    // Bypass the universal pre-command sweep by writing files with
    // DIFFERENT content. Sweep is content-aware (cmp -s) and skips
    // divergent pairs; archive then refuses with ARCHIVE_CONFLICT.
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(join(coordRoot, 'alice', 'inbox', f), 'inbox-version');
    writeFileSync(join(coordRoot, 'alice', 'archive', f), 'archive-version');
    const r = await call({ filename: f });
    expect(errorCode(r)).toBe('ARCHIVE_CONFLICT');
    expect(errorPayload(r)?.details).toMatchObject({
      identity: 'alice',
      filename: f,
    });
    // Both copies preserved.
    expect(existsSync(join(coordRoot, 'alice', 'inbox', f))).toBe(true);
    expect(existsSync(join(coordRoot, 'alice', 'archive', f))).toBe(true);
  });

  it('invalid filename grammar → INVALID_FILENAME', async () => {
    const r = await call({ filename: 'garbage' });
    expect(errorCode(r)).toBe('INVALID_FILENAME');
  });

  it('legacy 3-segment filename is accepted as outside .md → MESSAGE_NOT_FOUND when absent (task #128)', async () => {
    // Post task #128: a legacy 3-segment `.md` (or any off-format
    // `.md`) is valid to *reference* — it's the "outside message"
    // path. When the file doesn't exist, the error is
    // MESSAGE_NOT_FOUND, not INVALID_FILENAME.
    const r = await call({ filename: '1714826789010-myobie-aaaaaa.md' });
    expect(errorCode(r)).toBe('MESSAGE_NOT_FOUND');
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

  it('reserved identity name → INVALID_IDENTITY', async () => {
    const r = await call({
      filename: '1714826789010-aaaaaa.md',
      identity: 'inbox',
    });
    expect(errorCode(r)).toBe('INVALID_IDENTITY');
  });

  it('every error response carries content + _meta["coord/error"] + isError', async () => {
    const r = await call({ filename: '1714826789010-aaaaaa.md' });
    expect(r.isError).toBe(true);
    expect(r.content?.[0]?.text).toMatch(/^MESSAGE_NOT_FOUND:/);
    expect(errorPayload(r)).toMatchObject({
      code: 'MESSAGE_NOT_FOUND',
      message: expect.stringContaining('not found in inbox or archive'),
    });
    expect(r.structuredContent).toBeUndefined();
  });
});

// ─── Roundtrip via send + archive ─────────────────────────────────────

describe('st_msg_archive — roundtrip with send', () => {
  it('st_msg_send → st_msg_archive moves the file', async () => {
    const send = (await client.callTool({
      name: 'st_msg_send',
      arguments: { to: 'bob', body: 'msg' },
    })) as CallResult;
    const filename = send.structuredContent?.filename as string;
    const r = await call({ filename, identity: 'bob' });
    expect(r.structuredContent?.outcome).toBe('moved');
    expect(existsSync(join(coordRoot, 'bob', 'inbox', filename))).toBe(false);
    expect(existsSync(join(coordRoot, 'bob', 'archive', filename))).toBe(true);
  });

  it('a second archive call on the same file → outcome=idempotent', async () => {
    const send = (await client.callTool({
      name: 'st_msg_send',
      arguments: { to: 'bob', body: 'msg' },
    })) as CallResult;
    const filename = send.structuredContent?.filename as string;
    await call({ filename, identity: 'bob' }); // first archive: moved
    const r = await call({ filename, identity: 'bob' }); // second: no-op
    expect(r.structuredContent?.outcome).toBe('idempotent');
  });
});
