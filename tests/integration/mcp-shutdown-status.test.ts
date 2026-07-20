// tests/integration/mcp-shutdown-status.test.ts — MCP server writes
// `offline` to its identity's status file on SIGTERM / SIGINT.
//
// brief-022 task 2: peers reading my status must see the right value
// as soon as I die. A subprocess test is the only way to verify this
// — we need a real OS signal + a real process exit. In-memory
// transports won't reach the signal handlers.

import { spawn } from 'node:child_process';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, '..', '..');
const ST_BIN = join(REPO_ROOT, 'bin', 'st');

let scratch: string;
let stRoot: string;

beforeEach(() => {
  scratch = mkdtempSync('/tmp/st-mcp-shutdown-');
  stRoot = join(scratch, 'smalltalk');
  mkdirSync(join(stRoot, 'alice'), { recursive: true });
});

afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

async function attemptShutdown(
  signal: 'SIGTERM' | 'SIGINT',
  readyMs: number,
): Promise<string> {
  // Pre-seed status: this is what peers see right now.
  writeFileSync(join(stRoot, 'alice', 'status'), 'available\n');

  const proc = spawn(ST_BIN, ['mcp'], {
    env: {
      ...process.env,
      ST_ROOT: stRoot,
      ST_AGENT: 'alice',
    },
    stdio: ['pipe', 'pipe', 'pipe'],
  });

  // Wait for the server to finish booting before signalling. The signal
  // handlers are installed at the very top of `runWith` (before the
  // `mcp.connect` / channel-watcher awaits), but reaching `runWith` at
  // all costs node's strip-types compilation of the whole import graph,
  // which is ~500ms+ on newer node and varies by machine/load. A single
  // fixed delay is therefore flaky — the retry wrapper below escalates
  // this budget instead. Sending an `initialize` handshake would be a
  // precise ready-signal but would drag the MCP SDK into this test.
  await new Promise((r) => setTimeout(r, readyMs));

  proc.kill(signal);

  await new Promise<void>((res, rej) => {
    const tooLate = setTimeout(() => rej(new Error('subprocess hung')), 5000);
    proc.once('exit', () => {
      clearTimeout(tooLate);
      res();
    });
  });

  const statusFile = join(stRoot, 'alice', 'status');
  if (!existsSync(statusFile)) {
    throw new Error('status file missing after shutdown');
  }
  return readFileSync(statusFile, 'utf8').trim();
}

// Bounded retry over an escalating boot budget instead of one fixed
// sleep: offline-on-shutdown is deterministic once the handlers are
// installed, so a correct server lands `offline` on the first attempt
// whose budget clears its boot cost; a genuinely broken server exhausts
// every budget and still fails loud (returns the last observed status).
async function bootAndSignal(signal: 'SIGTERM' | 'SIGINT'): Promise<string> {
  let last = 'available';
  for (const readyMs of [700, 1500, 3000]) {
    last = await attemptShutdown(signal, readyMs);
    if (last === 'offline') return last;
  }
  return last;
}

describe('st mcp — shutdown writes `offline` to status', () => {
  it('SIGTERM flips status from `available` to `offline`', async () => {
    const finalStatus = await bootAndSignal('SIGTERM');
    expect(finalStatus).toBe('offline');
  }, 20_000);

  it('SIGINT flips status from `available` to `offline`', async () => {
    const finalStatus = await bootAndSignal('SIGINT');
    expect(finalStatus).toBe('offline');
  }, 20_000);
});
