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
const COORD_BIN = join(REPO_ROOT, 'bin', 'st');

let scratch: string;
let stRoot: string;

beforeEach(() => {
  scratch = mkdtempSync('/tmp/coord-mcp-shutdown-');
  stRoot = join(scratch, 'coord');
  mkdirSync(join(stRoot, 'alice'), { recursive: true });
});

afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

async function bootAndSignal(signal: 'SIGTERM' | 'SIGINT'): Promise<string> {
  // Pre-seed status: this is what peers see right now.
  writeFileSync(join(stRoot, 'alice', 'status'), 'available\n');

  const proc = spawn(COORD_BIN, ['mcp'], {
    env: {
      ...process.env,
      ST_ROOT: stRoot,
      ST_AGENT: 'alice',
    },
    stdio: ['pipe', 'pipe', 'pipe'],
  });

  // Wait for the server to be alive enough that the signal handlers are
  // installed (which happens inside runWith, after mcp.connect). A
  // small fixed delay is robust; the alternative — send an `initialize`
  // request — would mean dragging the MCP SDK in here just to time a
  // signal. The shutdown path's correctness doesn't depend on this
  // delay being precise.
  await new Promise((r) => setTimeout(r, 300));

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

describe('coord mcp — shutdown writes `offline` to status', () => {
  it('SIGTERM flips status from `available` to `offline`', async () => {
    const finalStatus = await bootAndSignal('SIGTERM');
    expect(finalStatus).toBe('offline');
  }, 15_000);

  it('SIGINT flips status from `available` to `offline`', async () => {
    const finalStatus = await bootAndSignal('SIGINT');
    expect(finalStatus).toBe('offline');
  }, 15_000);
});
