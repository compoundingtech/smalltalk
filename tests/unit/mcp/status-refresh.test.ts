// tests/unit/mcp/status-refresh.test.ts — periodic mtime refresh while
// the MCP server is running.
//
// brief-023: a healthy idle agent should not surface as `unknown` to
// peers just because the boot ritual wrote the status once and nothing
// has touched it since. While the server runs, it re-writes the
// current recorded value on STATUS_REFRESH_MS intervals to bump mtime.
//
// Tests use a small statusRefreshIntervalMs (50ms) so the refresh
// happens in test time rather than real minutes.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  utimesSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { createMcpServer } from '../../../src/mcp/index.ts';
import { asIdentity } from '../../../src/types.ts';

let scratch: string;
let stRoot: string;
let statusFile: string;

function makeHandle(opts: { refreshMs?: number } = {}) {
  return createMcpServer({
    root: stRoot,
    identity: asIdentity('alice'),
    ...(opts.refreshMs !== undefined && {
      statusRefreshIntervalMs: opts.refreshMs,
    }),
  });
}

async function connect(handle: ReturnType<typeof createMcpServer>): Promise<{
  client: Client;
  run: Promise<void>;
}> {
  const client = new Client({ name: 'test-refresh', version: '1.0' });
  const [c, s] = InMemoryTransport.createLinkedPair();
  // Drive lifecycle through runWith so startStatusRefresh fires.
  // runWith awaits the server until the transport closes, so we
  // capture the promise without awaiting it here.
  const run = handle.runWith(s);
  await client.connect(c);
  return { client, run };
}

async function wait(ms: number): Promise<void> {
  await new Promise((r) => setTimeout(r, ms));
}

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'st-status-refresh-'));
  stRoot = join(scratch, 'smalltalk');
  mkdirSync(join(stRoot, 'alice'), { recursive: true });
  statusFile = join(stRoot, 'alice', 'status');
});

afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

describe('MCP status refresh — preserves recorded value', () => {
  it('refresh tick re-writes `available` with a fresher mtime', async () => {
    writeFileSync(statusFile, 'available\n');
    // Backdate the file so the mtime change is observable even on
    // filesystems with coarse mtime resolution.
    const oldT = Date.now() - 10_000;
    utimesSync(statusFile, new Date(oldT), new Date(oldT));
    const mtimeBefore = statSync(statusFile).mtimeMs;

    const handle = makeHandle({ refreshMs: 30 });
    const { client, run } = await connect(handle);

    // Wait long enough for at least one tick.
    await wait(100);

    expect(readFileSync(statusFile, 'utf8').trim()).toBe('available');
    expect(statSync(statusFile).mtimeMs).toBeGreaterThan(mtimeBefore);

    await client.close();
    await handle.close();
    await run;
  });

  it('refresh preserves `busy` instead of clobbering with `available`', async () => {
    writeFileSync(statusFile, 'busy\n');
    const handle = makeHandle({ refreshMs: 30 });
    const { client, run } = await connect(handle);

    await wait(100);

    expect(readFileSync(statusFile, 'utf8').trim()).toBe('busy');

    await client.close();
    await handle.close();
    await run;
  });

  it('preserves `dnd`', async () => {
    writeFileSync(statusFile, 'dnd\n');
    const handle = makeHandle({ refreshMs: 30 });
    const { client, run } = await connect(handle);
    await wait(100);
    expect(readFileSync(statusFile, 'utf8').trim()).toBe('dnd');
    await client.close();
    await handle.close();
    await run;
  });
});

describe('MCP status refresh — edge cases', () => {
  it('missing file → writes `available` on first tick', async () => {
    expect(existsSync(statusFile)).toBe(false);
    const handle = makeHandle({ refreshMs: 30 });
    const { client, run } = await connect(handle);

    await wait(100);

    expect(existsSync(statusFile)).toBe(true);
    expect(readFileSync(statusFile, 'utf8').trim()).toBe('available');

    await client.close();
    await handle.close();
    await run;
  });

  it('corrupt contents → leaves the file untouched (no crash)', async () => {
    writeFileSync(statusFile, 'garbage-value\n');
    const handle = makeHandle({ refreshMs: 30 });
    const { client, run } = await connect(handle);

    await wait(100);

    // Same bytes as we wrote — the refresh refused to touch it.
    expect(readFileSync(statusFile, 'utf8').trim()).toBe('garbage-value');

    await client.close();
    await handle.close();
    await run;
  });

  it('literal `unknown` on disk → leaves the file untouched', async () => {
    // `unknown` should never be written by the system, but if it
    // somehow appears (manual edit, future bug), don't refresh it —
    // we'd be re-asserting a value we never trusted.
    writeFileSync(statusFile, 'unknown\n');
    const handle = makeHandle({ refreshMs: 30 });
    const { client, run } = await connect(handle);

    await wait(100);

    expect(readFileSync(statusFile, 'utf8').trim()).toBe('unknown');

    await client.close();
    await handle.close();
    await run;
  });
});

describe('MCP status refresh — shutdown ordering', () => {
  it('shutdown writes `offline` even when a refresh tick was in flight', async () => {
    writeFileSync(statusFile, 'available\n');
    const handle = makeHandle({ refreshMs: 30 });
    const { client, run } = await connect(handle);

    // Let the refresh tick fire a few times before we tear down.
    await wait(100);

    // Closing should: (a) stop the refresh interval, (b) write
    // `offline`. If the order is wrong, a tick fires after the
    // offline-write and we'd see `available` back on disk.
    await client.close();
    await handle.close();
    await run;

    // Give Node's event loop a beat to drain any straggler timer
    // callbacks that should have been cancelled. If our ordering is
    // correct, this is a no-op.
    await wait(80);

    expect(readFileSync(statusFile, 'utf8').trim()).toBe('offline');
  });

  it('refreshMs = 0 disables the refresh entirely', async () => {
    writeFileSync(statusFile, 'available\n');
    const oldT = Date.now() - 10_000;
    utimesSync(statusFile, new Date(oldT), new Date(oldT));
    const mtimeBefore = statSync(statusFile).mtimeMs;

    const handle = makeHandle({ refreshMs: 0 });
    const { client, run } = await connect(handle);

    await wait(100);

    // No refresh fired → mtime is unchanged (until close writes
    // offline, which we haven't called yet).
    expect(statSync(statusFile).mtimeMs).toBe(mtimeBefore);

    await client.close();
    await handle.close();
    await run;
  });
});
