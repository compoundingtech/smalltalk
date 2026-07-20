// tests/unit/mcp/lifecycle.test.ts — server construction, capability
// declaration, tools/list shape, connect+close, drift guard.
//
// Uses the SDK's InMemoryTransport.createLinkedPair() to wire a Client +
// the createMcpServer() handle in the same vitest worker. No subprocess.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import { mkdirSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  EXPECTED_TOOL_NAMES,
  SERVER_INFO,
  SERVER_OPTIONS,
} from '../../../src/mcp/capabilities.ts';
import { createMcpServer } from '../../../src/mcp/index.ts';
import { asIdentity } from '../../../src/types.ts';

let scratch: string;
let stRoot: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'st-mcp-lifecycle-'));
  stRoot = join(scratch, 'st');
  mkdirSync(stRoot, { recursive: true });
  mkdirSync(join(stRoot, 'alice', 'inbox'), { recursive: true });
  mkdirSync(join(stRoot, 'alice', 'archive'), { recursive: true });
});
afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

async function connectInMemory(opts?: { identity?: string }): Promise<{
  client: Client;
  handle: ReturnType<typeof createMcpServer>;
}> {
  const handle = createMcpServer({
    root: stRoot,
    identity: asIdentity(opts?.identity ?? 'alice'),
  });
  const client = new Client({ name: 'test-client', version: '1.0' });
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await Promise.all([
    client.connect(clientTransport),
    handle.mcp.connect(serverTransport),
  ]);
  return { client, handle };
}

// ─── Construction ──────────────────────────────────────────────────────

describe('createMcpServer — construction', () => {
  it('returns a handle with mcp + st + run + close', () => {
    const handle = createMcpServer({
      root: stRoot,
      identity: asIdentity('alice'),
    });
    expect(handle.mcp).toBeDefined();
    expect(handle.st).toBeDefined();
    expect(typeof handle.run).toBe('function');
    expect(typeof handle.close).toBe('function');
  });

  it('threads root + identity through to the embedded St', () => {
    const handle = createMcpServer({
      root: stRoot,
      identity: asIdentity('alice'),
    });
    expect(handle.st.root).toBe(stRoot);
    expect(handle.st.identity).toBe('alice');
  });

  it('honors configRoot when supplied', () => {
    const cfg = join(scratch, 'cfg');
    mkdirSync(cfg);
    const handle = createMcpServer({
      root: stRoot,
      identity: asIdentity('alice'),
      configRoot: cfg,
    });
    expect(handle.st.configRoot).toBe(cfg);
  });

  it('throws if identity is invalid (asIdentity catches in createSt)', () => {
    expect(() =>
      createMcpServer({
        root: stRoot,
        identity: 'INVALID' as unknown as ReturnType<typeof asIdentity>,
      })
    ).toThrowError(/invalid (agent name|identity)/);
  });
});

// ─── Capability declaration ────────────────────────────────────────────

describe('createMcpServer — capability declaration', () => {
  it('SERVER_INFO carries the canonical name + version', () => {
    expect(SERVER_INFO.name).toBe('st');
    expect(typeof SERVER_INFO.version).toBe('string');
    expect(SERVER_INFO.version.length).toBeGreaterThan(0);
  });

  it('Phase 1 capabilities advertise tools, no experimental', () => {
    expect(SERVER_OPTIONS.capabilities).toEqual({ tools: {} });
    expect(
      (SERVER_OPTIONS.capabilities as { experimental?: unknown }).experimental
    ).toBeUndefined();
  });

  it('after initialize, the client sees the canonical name + version', async () => {
    const { client, handle } = await connectInMemory();
    try {
      const v = client.getServerVersion();
      expect(v?.name).toBe('st');
      expect(typeof v?.version).toBe('string');
    } finally {
      await handle.close();
    }
  });

  it('after initialize, client sees tools capability advertised', async () => {
    const { client, handle } = await connectInMemory();
    try {
      const caps = client.getServerCapabilities();
      expect(caps?.tools).toBeDefined();
    } finally {
      await handle.close();
    }
  });

  it('Phase 1: experimental.claude/channel is NOT advertised', async () => {
    const { client, handle } = await connectInMemory();
    try {
      const caps = client.getServerCapabilities();
      expect(caps?.experimental).toBeUndefined();
    } finally {
      await handle.close();
    }
  });
});

// ─── tools/list (deferred to task 3) ───────────────────────────────────
//
// The SDK's McpServer installs the tool-request handlers lazily, the
// first time `registerTool` is called. With Phase-1 task-1's empty
// server, `client.listTools()` fails with "Method not found". The
// per-tool tests in tasks 3–7 cover the populated-server case.

describe('createMcpServer — tool name registry', () => {
  it('EXPECTED_TOOL_NAMES is the canonical non-channel tool set — `st_*` only (st_* alias removed post-cutover)', () => {
    expect([...EXPECTED_TOOL_NAMES]).toEqual([
      'st_msg_send',
      'st_msg_ls',
      'st_msg_read',
      'st_msg_archive',
      'st_msg_thread',
      'st_agents',
      'st_resource_add',
      'st_resource_ls',
      'st_resource_read',
      'st_resource_remove',
      'st_context_read',
      'st_context_write',
      'st_context_append',
    ]);
  });
});

// ─── Connect + close ───────────────────────────────────────────────────

describe('createMcpServer — connect + close', () => {
  it('close is idempotent (calling twice does not throw)', async () => {
    const { handle } = await connectInMemory();
    await handle.close();
    await expect(handle.close()).resolves.not.toThrow();
  });
});
