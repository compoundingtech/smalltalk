// tests/integration/ding.test.ts — st ding daemon end-to-end.
//
// Spawns a real `pty` session running a passive bash echoer, runs
// `st ding` against it as a child process, drops files into the
// watched identity's inbox, and asserts the keystrokes arrive at
// the bash session's screen via `pty peek`.
//
// Skipped when `pty` isn't on $PATH.
//
// PTY_SESSION_DIR isolation: the global vitest setup at
// tests/setup/pty-isolation.ts rewrites process.env.PTY_SESSION_DIR
// to a temp dir before this file imports. Every spawn here either
// omits `env:` (so the child inherits the isolated PTY_SESSION_DIR
// via Node's default) or spreads `...process.env` first (so it
// survives the override). DO NOT hardcode or strip PTY_SESSION_DIR
// in any spawn here — see brief-020 + CONTRIBUTING.md.

import { spawn, spawnSync } from 'node:child_process';
import {
  mkdirSync,
  mkdtempSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, '..', '..');
const ST_BIN = join(REPO_ROOT, 'bin', 'st');

function ptyAvailable(): boolean {
  return spawnSync('pty', ['--help'], { stdio: 'ignore' }).status === 0;
}
const HAS_PTY = ptyAvailable();

let scratch: string;
let stRoot: string;
let sessionName: string;
let dingProc: ReturnType<typeof spawn> | undefined;

function uniqueSessionName(): string {
  return `st-ding-it-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

function ptyExec(args: string[]): { stdout: string; stderr: string; status: number } {
  const r = spawnSync('pty', args, { encoding: 'utf8', timeout: 15_000 });
  return {
    stdout: typeof r.stdout === 'string' ? r.stdout : '',
    stderr: typeof r.stderr === 'string' ? r.stderr : '',
    status: r.status ?? -1,
  };
}

async function waitForText(
  needle: string,
  timeoutMs = 8000
): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  let last = '';
  while (Date.now() < deadline) {
    const r = ptyExec(['peek', '--plain', '--full', sessionName]);
    last = r.stdout;
    if (last.includes(needle)) return last;
    await new Promise((res) => setTimeout(res, 200));
  }
  throw new Error(
    `waitForText: "${needle}" not seen in ${timeoutMs}ms; last screen:\n${last}`
  );
}

beforeEach(() => {
  // Fast-fail if the global pty isolation setup didn't run. Without
  // it, the test would spawn pty sessions into the user's real
  // session dir — exactly the leak brief-020 was written to prevent.
  if (!process.env.PTY_SESSION_DIR) {
    throw new Error(
      'PTY_SESSION_DIR is not set — tests/setup/pty-isolation.ts ' +
        'did not run. Refusing to spawn pty sessions into the ' +
        "user's real session dir."
    );
  }
  scratch = mkdtempSync(join(tmpdir(), 'st-ding-it-'));
  stRoot = join(scratch, 'smalltalk');
  for (const id of ['alice', 'bob']) {
    mkdirSync(join(stRoot, id, 'inbox'), { recursive: true });
    mkdirSync(join(stRoot, id, 'archive'), { recursive: true });
  }
  sessionName = uniqueSessionName();
  if (HAS_PTY) {
    // Background bash that just echoes whatever line it reads. The
    // ding daemon will pty-send lines into it; we observe via `pty peek`.
    const r = ptyExec([
      'run',
      '-d',
      '--name',
      sessionName,
      '--',
      'bash',
      '-c',
      'while IFS= read -r line; do echo "got: $line"; done',
    ]);
    if (r.status !== 0) {
      throw new Error(
        `pty run failed: status=${r.status} stderr=${r.stderr}`
      );
    }
  }
});

afterEach(async () => {
  if (dingProc !== undefined) {
    dingProc.kill('SIGINT');
    try {
      await new Promise<void>((resolve) => {
        if (dingProc === undefined) {
          resolve();
          return;
        }
        const tooLate = setTimeout(() => {
          dingProc?.kill('SIGKILL');
          resolve();
        }, 2000);
        dingProc.once('exit', () => {
          clearTimeout(tooLate);
          resolve();
        });
      });
    } catch {
      // best-effort
    }
    dingProc = undefined;
  }
  if (HAS_PTY) {
    spawnSync('pty', ['kill', sessionName], { stdio: 'ignore' });
  }
  rmSync(scratch, { recursive: true, force: true });
});

function startDing(opts: { interval?: number } = {}): void {
  const args = [
    'ding',
    sessionName,
    '--identity',
    'bob',
    ...(opts.interval !== undefined
      ? ['--interval', String(opts.interval)]
      : []),
  ];
  dingProc = spawn(ST_BIN, args, {
    env: {
      ...process.env,
      ST_ROOT: stRoot,
      ST_AGENT: 'bob',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
}

function plant(
  identity: string,
  filename: string,
  fm: Record<string, string>,
  body: string
): void {
  const head = Object.entries(fm)
    .map(([k, v]) => `${k}: ${v}`)
    .join('\n');
  writeFileSync(
    join(stRoot, identity, 'inbox', filename),
    `---\n${head}\n---\n${body}\n`
  );
}

describe.skipIf(!HAS_PTY)('st ding — end-to-end with a real pty session', () => {
  it('available status: a new inbox file pty-sends a notice into the session', async () => {
    startDing({ interval: 200 });
    // Give the daemon a moment to attach to the watcher (sinceNow
    // means it ignores anything in inbox before this point).
    await new Promise((res) => setTimeout(res, 500));

    plant('bob', '1714826789010-aaaaaa.md', {
      from: 'alice',
      subject: 'deploy',
    }, 'how do we deploy?');

    const screen = await waitForText('got: [DING] new smalltalk message', 12_000);
    expect(screen).toContain('deploy');
    expect(screen).toContain('alice');
  }, 30_000);

  it('busy status: notice is buffered; flips to available → buffered notice arrives', async () => {
    // Set status to busy BEFORE starting the daemon so the watcher
    // sees `busy` on the first arrival.
    spawnSync(ST_BIN, ['status', 'bob', '--set', 'busy'], {
      env: { ...process.env, ST_ROOT: stRoot },
    });

    startDing({ interval: 150 });
    await new Promise((res) => setTimeout(res, 500));

    plant('bob', '1714826789020-bbbbbb.md', {
      from: 'alice',
      subject: 'urgent',
    }, 'urgent body');

    // Daemon should NOT have delivered yet — wait long enough for at
    // least one status-tick.
    await new Promise((res) => setTimeout(res, 1000));
    const beforeFlip = ptyExec(['peek', '--plain', '--full', sessionName]).stdout;
    expect(beforeFlip).not.toContain('[DING] new smalltalk message');

    // Flip to available.
    spawnSync(ST_BIN, ['status', 'bob', '--set', 'available'], {
      env: { ...process.env, ST_ROOT: stRoot },
    });

    const screen = await waitForText('got: [DING] new smalltalk message', 8000);
    expect(screen).toContain('urgent');
  }, 30_000);
});
