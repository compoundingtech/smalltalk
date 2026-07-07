// tests/unit/mcp/channel.test.ts — channel mode capability + opt-in flag.
//
// Phase-2 task 1 of brief-010: covers the `--channel` opt-in.
//   - Without the flag: capabilities are Phase-1 (no experimental).
//   - With the flag: experimental['claude/channel'] is advertised and
//     the server attaches an instructions string mentioning st_msg_reply.
//   - The flag must not break the existing five tools.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import { mkdirSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  buildServerOptions,
  CHANNEL_INSTRUCTIONS,
  EXPECTED_TOOL_NAMES,
  SERVER_OPTIONS,
} from '../../../src/mcp/capabilities.ts';
import { createMcpServer } from '../../../src/mcp/index.ts';
import { asIdentity } from '../../../src/types.ts';

let scratch: string;
let coordRoot: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-mcp-channel-'));
  coordRoot = join(scratch, 'coord');
  for (const id of ['alice', 'bob']) {
    mkdirSync(join(coordRoot, id, 'inbox'), { recursive: true });
    mkdirSync(join(coordRoot, id, 'archive'), { recursive: true });
  }
});
afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

async function connect(channel: boolean): Promise<{
  client: Client;
  handle: ReturnType<typeof createMcpServer>;
}> {
  const handle = createMcpServer({
    root: coordRoot,
    identity: asIdentity('alice'),
    channel,
  });
  const client = new Client({ name: 'test-channel', version: '1.0' });
  const [c, s] = InMemoryTransport.createLinkedPair();
  await Promise.all([client.connect(c), handle.mcp.connect(s)]);
  return { client, handle };
}

// ─── buildServerOptions ───────────────────────────────────────────────

describe('buildServerOptions', () => {
  it('channel: false → Phase-1 options (tools only, no experimental, no instructions)', () => {
    const opts = buildServerOptions({ channel: false });
    expect(opts).toEqual(SERVER_OPTIONS);
    expect(opts.capabilities?.experimental).toBeUndefined();
    expect(opts.instructions).toBeUndefined();
  });

  it('channel: true → adds experimental["claude/channel"] and instructions', () => {
    const opts = buildServerOptions({ channel: true });
    expect(opts.capabilities?.tools).toBeDefined();
    expect(opts.capabilities?.experimental).toEqual({
      'claude/channel': {},
    });
    expect(opts.instructions).toBe(CHANNEL_INSTRUCTIONS);
  });

  it('CHANNEL_INSTRUCTIONS mentions st_msg_reply (so the agent knows the verb)', () => {
    expect(CHANNEL_INSTRUCTIONS).toMatch(/st_msg_reply/);
  });
});

// ─── Default mode (no --channel) ──────────────────────────────────────

describe('createMcpServer — default mode (no --channel)', () => {
  it('does NOT advertise experimental.claude/channel', async () => {
    const { client, handle } = await connect(false);
    try {
      const caps = client.getServerCapabilities();
      expect(caps?.experimental).toBeUndefined();
    } finally {
      await handle.close();
    }
  });

  it('still returns the Phase-1 five tools', async () => {
    const { client, handle } = await connect(false);
    try {
      const r = await client.listTools();
      const names = r.tools.map((t) => t.name).sort();
      expect(names).toEqual([...EXPECTED_TOOL_NAMES].sort());
    } finally {
      await handle.close();
    }
  });

  it('does not attach a server instructions string', async () => {
    const { client, handle } = await connect(false);
    try {
      // The SDK exposes instructions on the initialize result via
      // getInstructions() if any. With no instructions, it's undefined.
      expect(client.getInstructions?.()).toBeUndefined();
    } finally {
      await handle.close();
    }
  });
});

// ─── Channel mode (--channel) ─────────────────────────────────────────

describe('createMcpServer — channel mode (--channel)', () => {
  it('advertises experimental["claude/channel"] = {}', async () => {
    const { client, handle } = await connect(true);
    try {
      const caps = client.getServerCapabilities();
      expect(caps?.experimental).toEqual({ 'claude/channel': {} });
    } finally {
      await handle.close();
    }
  });

  it('attaches instructions that mention st_msg_reply', async () => {
    const { client, handle } = await connect(true);
    try {
      const ins = client.getInstructions?.();
      expect(typeof ins).toBe('string');
      expect(ins).toMatch(/st_msg_reply/);
    } finally {
      await handle.close();
    }
  });

  it('still serves the Phase-1 five tools without regression (st_msg_reply lands in task 3)', async () => {
    const { client, handle } = await connect(true);
    try {
      const r = await client.listTools();
      const names = r.tools.map((t) => t.name).sort();
      // Task 1 only wires capabilities + flag. st_msg_reply isn't
      // registered yet — keep the test set narrow so task 3's commit
      // explicitly extends it.
      for (const phase1 of EXPECTED_TOOL_NAMES) {
        expect(names).toContain(phase1);
      }
    } finally {
      await handle.close();
    }
  });

  it('st_msg_send still works end-to-end with the channel flag on', async () => {
    const { client, handle } = await connect(true);
    try {
      const r = (await client.callTool({
        name: 'st_msg_send',
        arguments: { to: 'bob', body: 'hi via channel' },
      })) as {
        isError?: boolean;
        structuredContent?: { filename?: string };
      };
      expect(r.isError).toBeUndefined();
      expect(r.structuredContent?.filename).toMatch(
        /^[0-9]{13}-[0-9a-z]{6}\.md$/
      );
    } finally {
      await handle.close();
    }
  });
});
