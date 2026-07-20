// tests/unit/mcp/reply.test.ts — st_msg_reply tool (channel mode only).
//
// Phase-2 task 3 of brief-010. st_msg_reply is registered only when the
// server is constructed with `channel: true`; in default Phase-1 mode
// the tool is absent.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { createMcpServer } from '../../../src/mcp/index.ts';
import { asIdentity } from '../../../src/types.ts';

import { errorCode, errorPayload } from './_helpers.ts';

interface CallResult {
  isError?: boolean;
  content?: Array<{ type: string; text?: string }>;
  structuredContent?: Record<string, unknown>;
}

let scratch: string;
let stRoot: string;
let client: Client;
let handle: ReturnType<typeof createMcpServer>;

async function setup(opts: { channel: boolean; identity?: string }): Promise<void> {
  handle = createMcpServer({
    root: stRoot,
    identity: asIdentity(opts.identity ?? 'bob'),
    channel: opts.channel,
  });
  client = new Client({ name: 'test-reply', version: '1.0' });
  const [c, s] = InMemoryTransport.createLinkedPair();
  await Promise.all([client.connect(c), handle.mcp.connect(s)]);
}

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'st-mcp-reply-'));
  stRoot = join(scratch, 'smalltalk');
  for (const id of ['alice', 'bob', 'carol']) {
    mkdirSync(join(stRoot, id, 'inbox'), { recursive: true });
    mkdirSync(join(stRoot, id, 'archive'), { recursive: true });
  }
});
afterEach(async () => {
  await handle?.close();
  rmSync(scratch, { recursive: true, force: true });
});

function plant(
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

async function reply(args: Record<string, unknown>): Promise<CallResult> {
  return (await client.callTool({
    name: 'st_msg_reply',
    arguments: args,
  })) as CallResult;
}

function readReply(identity: string, filename: string): string {
  return readFileSync(join(stRoot, identity, 'inbox', filename), 'utf8');
}

// ─── Registration / mode gating ────────────────────────────────────────

describe('st_msg_reply — registration', () => {
  it('is NOT registered when channel mode is off', async () => {
    await setup({ channel: false });
    const r = await client.listTools();
    const names = r.tools.map((t) => t.name);
    expect(names).not.toContain('st_msg_reply');
  });

  it('IS registered when channel mode is on, with required {thread, body}', async () => {
    await setup({ channel: true });
    const r = await client.listTools();
    const tool = r.tools.find((t) => t.name === 'st_msg_reply');
    expect(tool).toBeDefined();
    expect(tool?.inputSchema?.required).toEqual(['thread', 'body']);
    expect(tool?.outputSchema).toBeDefined();
  });
});

// ─── Happy paths ───────────────────────────────────────────────────────

describe('st_msg_reply — happy paths', () => {
  beforeEach(async () => {
    await setup({ channel: true });
  });

  it('inbox → reply: writes a new file in the original sender\'s inbox', async () => {
    const orig = '1714826789010-aaaaaa.md';
    plant('bob', orig, { from: 'alice', subject: 'q' }, 'what is 2+2?');
    const r = await reply({ thread: orig, body: '4' });
    expect(r.isError).toBeUndefined();
    const sc = r.structuredContent as { filename: string; identity: string };
    expect(sc.identity).toBe('alice');
    expect(sc.filename).toMatch(/^[0-9]{13}-[0-9a-z]{6}\.md$/);
    const written = readReply('alice', sc.filename);
    expect(written).toContain('from: bob');
    expect(written).toContain(`in-reply-to: ${orig}`);
    expect(written).toContain('subject:');
    expect(written).toContain('re: q');
    expect(written).toContain('4\n');
  });

  it('content[0].text summarizes the reply destination', async () => {
    const orig = '1714826789020-bbbbbb.md';
    plant('bob', orig, { from: 'alice' }, 'hi');
    const r = await reply({ thread: orig, body: 'hello' });
    expect(r.content?.[0]?.text).toMatch(
      /^replied: alice\/[0-9]{13}-[0-9a-z]{6}\.md$/
    );
  });

  it('reply to an archived message resolves to the archived sender', async () => {
    const orig = '1714826789030-cccccc.md';
    plant(
      'bob',
      orig,
      { from: 'alice', subject: 'old' },
      'archived msg',
      'archive'
    );
    const r = await reply({ thread: orig, body: 'late reply' });
    expect(r.isError).toBeUndefined();
    const sc = r.structuredContent as { identity: string };
    expect(sc.identity).toBe('alice');
  });

  it('cross-identity: reply to a message that lives in a peer\'s archive', async () => {
    // Set up a synced peer archive: carol's archive holds a message
    // from alice. bob's tree doesn't contain the file at all — yet
    // st_msg_reply locates it via the peer-archive scan.
    const orig = '1714826789040-dddddd.md';
    plant('carol', orig, { from: 'alice', subject: 'peer' }, 'peer msg', 'archive');
    const r = await reply({ thread: orig, body: 'thanks' });
    expect(r.isError).toBeUndefined();
    const sc = r.structuredContent as { identity: string; filename: string };
    expect(sc.identity).toBe('alice');
    expect(existsSync(join(stRoot, 'alice', 'inbox', sc.filename))).toBe(true);
  });

  it('explicit subject overrides the auto-derived re: form', async () => {
    const orig = '1714826789050-eeeeee.md';
    plant('bob', orig, { from: 'alice', subject: 'q' }, 'q?');
    const r = await reply({
      thread: orig,
      body: 'a',
      subject: 'completely different',
    });
    const sc = r.structuredContent as { filename: string };
    const written = readReply('alice', sc.filename);
    expect(written).toContain('subject:');
    expect(written).toContain('completely different');
    expect(written).not.toContain('re: q');
  });

  it('original without subject → no subject line on the reply', async () => {
    const orig = '1714826789060-ffffff.md';
    plant('bob', orig, { from: 'alice' }, 'no subject here');
    const r = await reply({ thread: orig, body: 'ack' });
    const sc = r.structuredContent as { filename: string };
    const written = readReply('alice', sc.filename);
    expect(written).not.toContain('subject:');
  });
});

// ─── Typed errors ──────────────────────────────────────────────────────

describe('st_msg_reply — typed error mapping', () => {
  beforeEach(async () => {
    await setup({ channel: true });
  });

  it('unknown thread filename → MESSAGE_NOT_FOUND', async () => {
    const r = await reply({
      thread: '1714826789999-zzzzzz.md',
      body: 'orphan',
    });
    expect(errorCode(r)).toBe('MESSAGE_NOT_FOUND');
    expect(errorPayload(r)?.details).toMatchObject({
      filename: '1714826789999-zzzzzz.md',
    });
  });

  it('invalid thread grammar → INVALID_FILENAME', async () => {
    const r = await reply({ thread: 'garbage', body: 'x' });
    expect(errorCode(r)).toBe('INVALID_FILENAME');
  });

  it('legacy 3-segment filename → INVALID_FILENAME', async () => {
    // Legacy filenames included a middle name segment (`<unix-ms>-
    // <sender>-<rand6>.md`) — brief-017 dropped that back to a
    // 2-segment grammar. Any 3+ segment filename hits the validator.
    const r = await reply({
      thread: '1714826789010-legacy-aaaaaa.md',
      body: 'x',
    });
    expect(errorCode(r)).toBe('INVALID_FILENAME');
  });

  it('empty body → EMPTY_BODY', async () => {
    const orig = '1714826789070-gggggg.md';
    plant('bob', orig, { from: 'alice' }, 'q');
    const r = await reply({ thread: orig, body: '' });
    expect(errorCode(r)).toBe('EMPTY_BODY');
  });

  it('thread file has no `from:` field → INVALID_IDENTITY', async () => {
    const orig = '1714826789080-hhhhhh.md';
    // Frontmatter fence with no `from`; st_msg_reply has no recipient
    // to send to.
    writeFileSync(
      join(stRoot, 'bob', 'inbox', orig),
      '---\nsubject: orphan\n---\nbody\n'
    );
    const r = await reply({ thread: orig, body: 'x' });
    expect(errorCode(r)).toBe('INVALID_IDENTITY');
  });
});

// ─── Side-effect isolation ────────────────────────────────────────────

describe('st_msg_reply — side effects', () => {
  beforeEach(async () => {
    await setup({ channel: true });
  });

  it('does not leave any file in bob\'s inbox/archive (only writes to alice)', async () => {
    const orig = '1714826789090-iiiiii.md';
    plant('bob', orig, { from: 'alice' }, 'q');
    await reply({ thread: orig, body: 'a' });
    // bob's inbox has only the original message; archive empty.
    expect(readdirSync(join(stRoot, 'bob', 'inbox'))).toEqual([orig]);
    expect(readdirSync(join(stRoot, 'bob', 'archive'))).toEqual([]);
    // alice received exactly one new file.
    expect(readdirSync(join(stRoot, 'alice', 'inbox'))).toHaveLength(1);
  });
});

// ─── Concurrency invariant pin-down ────────────────────────────────────
//
// An operator observed two parallel `st_msg_reply` calls landing in
// the recipient's *archive* instead of *inbox*, alongside three
// concurrent `st_msg_archive` calls on the sender's tree.
// Investigation: the MCP send path writes only to inboxDir(to, root)
// (see commands/send.ts cmdSend) and the pre-command sweep only
// DELETES inbox files that have a byte-identical twin already in
// archive (common.ts:sweep). There is no MCP path that writes to or
// moves into archive. The anomaly almost certainly came from an
// external UI clicking through messages. These tests pin every
// adjacent invariant so a future refactor can't reintroduce the
// worry.

describe('st_msg_reply — invariants under archive concurrency (brief-035)', () => {
  beforeEach(async () => {
    await setup({ channel: true });
  });

  it('the reply file lands in alice/inbox, NEVER alice/archive', async () => {
    const orig = '1714826789010-aaaaaa.md';
    plant('bob', orig, { from: 'alice' }, 'q');
    const r = await reply({ thread: orig, body: 'a' });
    expect(r.isError).toBeUndefined();
    const filename = r.structuredContent?.filename as string;
    // Affirmative: in alice/inbox.
    expect(existsSync(join(stRoot, 'alice', 'inbox', filename))).toBe(
      true
    );
    // Negative: NOT in alice/archive.
    expect(existsSync(join(stRoot, 'alice', 'archive', filename))).toBe(
      false
    );
  });

  it('written file carries inReplyTo and the derived re: subject', async () => {
    const orig = '1714826789010-aaaaaa.md';
    plant(
      'bob',
      orig,
      { from: 'alice', subject: 'deploy plan' },
      'thoughts?'
    );
    const r = await reply({ thread: orig, body: 'looks good' });
    const filename = r.structuredContent?.filename as string;
    const text = readReply('alice', filename);
    expect(text).toContain('from: bob');
    expect(text).toContain(`in-reply-to: ${orig}`);
    expect(text).toContain('subject: "re: deploy plan"');
    expect(text).toContain('looks good');
  });

  it('concurrent archives on sender tree do not divert the reply', async () => {
    // Reproduces the brief-035 anomaly shape: while two replies are in
    // flight, three archive ops on the SENDER's (bob's) inbox run in
    // parallel. The MCP server processes parallel calls on the same
    // event loop with sync FS ops, but we exercise the await
    // interleaving via Promise.all.
    //
    // Plant: three messages in bob's inbox (to be archived in parallel)
    // and two messages from alice (to be replied to in parallel).
    plant('bob', '1714826789010-aaaaaa.md', { from: 'carol' }, 'msg1');
    plant('bob', '1714826789020-bbbbbb.md', { from: 'carol' }, 'msg2');
    plant('bob', '1714826789030-cccccc.md', { from: 'carol' }, 'msg3');
    plant('bob', '1714826789040-dddddd.md', { from: 'alice' }, 'q1');
    plant('bob', '1714826789050-eeeeee.md', { from: 'alice' }, 'q2');

    const aliceInboxBefore = readdirSync(
      join(stRoot, 'alice', 'inbox')
    ).length;
    const aliceArchiveBefore = readdirSync(
      join(stRoot, 'alice', 'archive')
    ).length;

    const archive = (filename: string) =>
      client.callTool({
        name: 'st_msg_archive',
        arguments: { filename },
      });
    const replyCall = (thread: string) =>
      client.callTool({
        name: 'st_msg_reply',
        arguments: { thread, body: 'reply' },
      });

    // All five calls in parallel, mirroring the anomaly shape.
    const results = await Promise.all([
      archive('1714826789010-aaaaaa.md'),
      archive('1714826789020-bbbbbb.md'),
      archive('1714826789030-cccccc.md'),
      replyCall('1714826789040-dddddd.md'),
      replyCall('1714826789050-eeeeee.md'),
    ]);

    // No errors.
    for (const r of results) {
      expect((r as CallResult).isError).toBeUndefined();
    }

    // alice receives EXACTLY two replies, both in inbox, zero in archive.
    const aliceInboxAfter = readdirSync(join(stRoot, 'alice', 'inbox'));
    const aliceArchiveAfter = readdirSync(
      join(stRoot, 'alice', 'archive')
    );
    expect(aliceInboxAfter.length - aliceInboxBefore).toBe(2);
    expect(aliceArchiveAfter.length - aliceArchiveBefore).toBe(0);

    // bob's tree: the three archived messages are now in archive, the
    // two alice messages remain in inbox (since they weren't archived,
    // only replied to).
    expect(readdirSync(join(stRoot, 'bob', 'archive')).sort()).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789020-bbbbbb.md',
      '1714826789030-cccccc.md',
    ]);
    expect(readdirSync(join(stRoot, 'bob', 'inbox')).sort()).toEqual([
      '1714826789040-dddddd.md',
      '1714826789050-eeeeee.md',
    ]);
  });

  it('a pre-existing unrelated file in alice/archive does not divert the reply', async () => {
    // Sweep-invariant guard: alice/archive has an unrelated file. The
    // new reply gets a fresh <ms>-<rand6> filename → no byte-twin in
    // archive → sweep leaves the new inbox file alone. The reply must
    // still land in inbox.
    plant(
      'alice',
      '1714826789000-zzzzzz.md',
      { from: 'someone' },
      'historical',
      'archive'
    );

    const orig = '1714826789010-aaaaaa.md';
    plant('bob', orig, { from: 'alice' }, 'q');
    const r = await reply({ thread: orig, body: 'a' });
    const filename = r.structuredContent?.filename as string;
    expect(existsSync(join(stRoot, 'alice', 'inbox', filename))).toBe(
      true
    );
    expect(existsSync(join(stRoot, 'alice', 'archive', filename))).toBe(
      false
    );
    // archive still has just the pre-existing unrelated file.
    expect(readdirSync(join(stRoot, 'alice', 'archive'))).toEqual([
      '1714826789000-zzzzzz.md',
    ]);
  });

  it('explicit subject override + no original subject → reply carries the override', async () => {
    const orig = '1714826789010-aaaaaa.md';
    plant('bob', orig, { from: 'alice' }, 'no subject in original');
    const r = await reply({
      thread: orig,
      body: 'a',
      subject: 'custom subject',
    });
    const filename = r.structuredContent?.filename as string;
    const text = readReply('alice', filename);
    expect(text).toContain('subject: "custom subject"');
  });
});
