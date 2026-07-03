// tests/unit/mcp/ls.test.ts — coord_msg_ls tool, in-memory.

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
let coordRoot: string;
let client: Client;
let handle: ReturnType<typeof createMcpServer>;

beforeEach(async () => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-mcp-ls-'));
  coordRoot = join(scratch, 'coord');
  for (const id of ['alice', 'bob']) {
    mkdirSync(join(coordRoot, id, 'inbox'), { recursive: true });
    mkdirSync(join(coordRoot, id, 'archive'), { recursive: true });
  }
  handle = createMcpServer({
    root: coordRoot,
    identity: asIdentity('alice'),
  });
  client = new Client({ name: 'test-ls', version: '1.0' });
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

async function call(args: Record<string, unknown> = {}): Promise<CallResult> {
  return (await client.callTool({
    name: 'coord_msg_ls',
    arguments: args,
  })) as CallResult;
}

function writeMsg(
  identity: string,
  filename: string,
  fromValue: string,
  body = 'body',
  folder: 'inbox' | 'archive' = 'inbox'
): void {
  writeFileSync(
    join(coordRoot, identity, folder, filename),
    `---\nfrom: ${fromValue}\n---\n${body}\n`
  );
}

// ─── Tools/list ────────────────────────────────────────────────────────

describe('coord_msg_ls — tools/list registration', () => {
  it('appears with input + output schemas', async () => {
    const r = await client.listTools();
    const tool = r.tools.find((t) => t.name === 'coord_msg_ls');
    expect(tool).toBeDefined();
    expect(tool?.inputSchema).toBeDefined();
    expect(tool?.outputSchema).toBeDefined();
  });

  it('all input fields are optional (zero-arg form valid)', async () => {
    const r = await client.listTools();
    const tool = r.tools.find((t) => t.name === 'coord_msg_ls');
    expect(tool?.inputSchema?.required ?? []).toEqual([]);
  });
});

// ─── Happy paths ───────────────────────────────────────────────────────

describe('coord_msg_ls — happy paths', () => {
  it('zero-arg lists the Coord identity\'s inbox', async () => {
    writeMsg('alice', '1714826789010-aaaaaa.md', 'bob');
    const r = await call();
    expect(r.isError).toBeUndefined();
    expect(r.structuredContent).toMatchObject({
      identity: 'alice',
      archive: false,
      matches: ['1714826789010-aaaaaa.md'],
    });
  });

  it('explicit identity overrides the default', async () => {
    writeMsg('bob', '1714826789010-aaaaaa.md', 'alice');
    const r = await call({ identity: 'bob' });
    expect(r.structuredContent?.identity).toBe('bob');
    expect(r.structuredContent?.matches).toEqual([
      '1714826789010-aaaaaa.md',
    ]);
  });

  it('archive=true lists the archive folder', async () => {
    writeMsg('alice', '1714826789010-aaaaaa.md', 'bob', 'live', 'inbox');
    writeMsg('alice', '1714826789020-bbbbbb.md', 'bob', 'old', 'archive');
    const r = await call({ archive: true });
    expect(r.structuredContent).toMatchObject({
      archive: true,
      matches: ['1714826789020-bbbbbb.md'],
    });
  });

  it('listing is in chronological (filename) order', async () => {
    writeMsg('alice', '1714826789030-cccccc.md', 'bob');
    writeMsg('alice', '1714826789010-aaaaaa.md', 'bob');
    writeMsg('alice', '1714826789020-bbbbbb.md', 'bob');
    const r = await call();
    expect(r.structuredContent?.matches).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789020-bbbbbb.md',
      '1714826789030-cccccc.md',
    ]);
  });

  it('empty inbox returns an empty matches array', async () => {
    const r = await call();
    expect(r.structuredContent?.matches).toEqual([]);
  });

  it('content[0].text pluralizes singular vs plural', async () => {
    const empty = await call();
    expect(empty.content?.[0]?.text).toBe('0 messages in inbox');
    writeMsg('alice', '1714826789010-aaaaaa.md', 'bob');
    const one = await call();
    expect(one.content?.[0]?.text).toBe('1 message in inbox');
    writeMsg('alice', '1714826789020-bbbbbb.md', 'bob');
    const two = await call();
    expect(two.content?.[0]?.text).toBe('2 messages in inbox');
  });
});

// ─── Filters ───────────────────────────────────────────────────────────

describe('coord_msg_ls — filters', () => {
  it('since filters by filename ts (>= cutoff)', async () => {
    writeMsg('alice', '1714826789010-aaaaaa.md', 'bob');
    writeMsg('alice', '1714826789020-bbbbbb.md', 'bob');
    writeMsg('alice', '1714826789030-cccccc.md', 'bob');
    const r = await call({ since: 1714826789020 });
    expect(r.structuredContent?.matches).toEqual([
      '1714826789020-bbbbbb.md',
      '1714826789030-cccccc.md',
    ]);
  });

  it('since=0 includes everything', async () => {
    writeMsg('alice', '1714826789010-aaaaaa.md', 'bob');
    const r = await call({ since: 0 });
    expect(r.structuredContent?.matches).toHaveLength(1);
  });

  it('fromFilter narrows to messages with that frontmatter from:', async () => {
    writeMsg('alice', '1714826789010-aaaaaa.md', 'bob');
    writeMsg('alice', '1714826789020-bbbbbb.md', 'carol');
    writeMsg('alice', '1714826789030-cccccc.md', 'bob');
    const r = await call({ fromFilter: 'bob' });
    expect(r.structuredContent?.matches).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789030-cccccc.md',
    ]);
  });

  it('combines since + fromFilter (intersection)', async () => {
    writeMsg('alice', '1714826789010-aaaaaa.md', 'bob');
    writeMsg('alice', '1714826789020-bbbbbb.md', 'bob');
    writeMsg('alice', '1714826789030-cccccc.md', 'carol');
    const r = await call({ since: 1714826789020, fromFilter: 'bob' });
    expect(r.structuredContent?.matches).toEqual([
      '1714826789020-bbbbbb.md',
    ]);
  });

  it('non-.md files (README) are silently skipped; outside .md files are listed (task #128)', async () => {
    writeMsg('alice', '1714826789010-aaaaaa.md', 'bob');
    writeFileSync(join(coordRoot, 'alice', 'inbox', 'README'), 'x');
    writeFileSync(join(coordRoot, 'alice', 'inbox', 'notes.md'), 'x');
    writeFileSync(
      join(coordRoot, 'alice', 'inbox', '1714826789020-myobie-aaaaaa.md'),
      'legacy'
    );
    const r = await call();
    // Canonical + both outside .md files show; README stays out.
    expect(r.structuredContent?.matches).toEqual(
      expect.arrayContaining([
        '1714826789010-aaaaaa.md',
        'notes.md',
        '1714826789020-myobie-aaaaaa.md',
      ])
    );
    expect(r.structuredContent?.matches).toHaveLength(3);
  });

  it('files with malformed frontmatter are silently excluded by fromFilter', async () => {
    writeMsg('alice', '1714826789010-aaaaaa.md', 'bob');
    writeFileSync(
      join(coordRoot, 'alice', 'inbox', '1714826789020-bbbbbb.md'),
      'no frontmatter'
    );
    const r = await call({ fromFilter: 'bob' });
    expect(r.structuredContent?.matches).toEqual([
      '1714826789010-aaaaaa.md',
    ]);
  });
});

// ─── Schema validation ─────────────────────────────────────────────────

describe('coord_msg_ls — schema validation', () => {
  it('rejects negative since', async () => {
    const r = await call({ since: -1 });
    expect(r.isError).toBe(true);
  });

  it('rejects since as a string', async () => {
    const r = await call({ since: 'abc' });
    expect(r.isError).toBe(true);
  });

  it('rejects archive as a string', async () => {
    const r = await call({ archive: 'yes' });
    expect(r.isError).toBe(true);
  });
});

// ─── Error mapping ─────────────────────────────────────────────────────

describe('coord_msg_ls — typed error mapping', () => {
  it('unknown identity → IDENTITY_NOT_HOSTED', async () => {
    const r = await call({ identity: 'ghost' });
    expect(errorCode(r)).toBe('IDENTITY_NOT_HOSTED');
    expect(errorPayload(r)?.details).toEqual({ identity: 'ghost' });
  });

  it('invalid identity grammar → INVALID_IDENTITY', async () => {
    const r = await call({ identity: 'INVALID' });
    expect(errorCode(r)).toBe('INVALID_IDENTITY');
  });

  it('invalid fromFilter identity → INVALID_IDENTITY', async () => {
    const r = await call({ fromFilter: 'INVALID' });
    expect(errorCode(r)).toBe('INVALID_IDENTITY');
  });
});

// ─── No inline presweep on non-sync paths (post sweep-as-convergence) ──

describe('coord_msg_ls — no inline presweep', () => {
  it('byte-identical inbox+archive twin is STILL visible after ls', async () => {
    // Per the sweep-as-convergence policy, ls does not presweep — a
    // zombie will surface in ls output until lazy-read sweep,
    // `coord sweep`, or a sync runs. (Test stays expressive: this
    // pins the new contract so a regression to inline presweep
    // would fail this assertion.)
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(join(coordRoot, 'alice', 'inbox', f), 'same');
    writeFileSync(join(coordRoot, 'alice', 'archive', f), 'same');
    const r = await call();
    expect(r.structuredContent?.matches).toEqual([f]);
    expect(
      require('node:fs').existsSync(join(coordRoot, 'alice', 'inbox', f))
    ).toBe(true);
  });
});
