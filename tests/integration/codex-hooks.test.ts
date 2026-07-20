// tests/integration/codex-hooks.test.ts — Codex SessionStart and
// Stop hook scripts at examples/codex/{session-start,stop}.sh.
//
// The scripts are bash; the tests run them as real subprocesses
// against a fixture smalltalk root. Skipped on hosts without `jq` on
// PATH (the scripts use jq to construct the JSON envelope).

import { spawnSync } from 'node:child_process';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, '..', '..');
const ST_BIN = join(REPO_ROOT, 'bin', 'smalltalk');
const SESSION_START_SH = join(
  REPO_ROOT,
  'examples',
  'codex',
  'session-start.sh'
);
const STOP_SH = join(REPO_ROOT, 'examples', 'codex', 'stop.sh');

function jqAvailable(): boolean {
  return spawnSync('jq', ['--version'], { stdio: 'ignore' }).status === 0;
}
const HAS_JQ = jqAvailable();

let scratch: string;
let stRoot: string;
let stateHome: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'st-it-codex-'));
  stRoot = join(scratch, 'smalltalk');
  for (const id of ['alice', 'bob']) {
    mkdirSync(join(stRoot, id, 'inbox'), { recursive: true });
    mkdirSync(join(stRoot, id, 'archive'), { recursive: true });
  }
  stateHome = join(scratch, 'state');
  mkdirSync(stateHome, { recursive: true });
});
afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

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

interface RunResult {
  stdout: string;
  stderr: string;
  exitCode: number;
}

function runHook(
  script: string,
  env: NodeJS.ProcessEnv
): RunResult {
  // Path-prepend bin/smalltalk and the system PATH so smalltalk + jq are
  // both reachable from within the hook script.
  const path = `${join(REPO_ROOT, 'bin')}:${process.env.PATH ?? ''}`;
  const fullEnv: NodeJS.ProcessEnv = {
    PATH: path,
    HOME: process.env.HOME,
    ...env,
  };
  const r = spawnSync('bash', [script], {
    env: fullEnv,
    encoding: 'utf8',
    timeout: 15_000,
  });
  return {
    stdout: typeof r.stdout === 'string' ? r.stdout : '',
    stderr: typeof r.stderr === 'string' ? r.stderr : '',
    exitCode: r.status ?? -1,
  };
}

// ─── session-start.sh ──────────────────────────────────────────────────

describe.skipIf(!HAS_JQ)('codex hooks — session-start.sh', () => {
  it('two files in inbox → JSON payload with additionalContext containing both', () => {
    plant('bob', '1714826789010-aaaaaa.md', {
      from: 'alice',
      subject: 'deploy question',
    }, 'how do we deploy?');
    plant('bob', '1714826789020-bbbbbb.md', { from: 'alice' }, 'ping');

    const r = runHook(SESSION_START_SH, {
      ST_ROOT: stRoot,
      ST_AGENT: 'bob',
    });

    expect(r.exitCode).toBe(0);
    const payload = JSON.parse(r.stdout) as {
      additionalContext: string;
      continue: boolean;
    };
    expect(payload.continue).toBe(true);
    expect(payload.additionalContext).toContain('## st inbox (2 unread)');
    expect(payload.additionalContext).toContain('1714826789010-aaaaaa.md');
    expect(payload.additionalContext).toContain('1714826789020-bbbbbb.md');
    expect(payload.additionalContext).toContain('Subject: deploy question');
    expect(payload.additionalContext).toContain('alice');
  });

  it('empty inbox → silent stdout, exit 0', () => {
    const r = runHook(SESSION_START_SH, {
      ST_ROOT: stRoot,
      ST_AGENT: 'bob',
    });
    expect(r.exitCode).toBe(0);
    expect(r.stdout).toBe('');
  });

  it('missing ST_ROOT → exit non-zero, stderr message, stdout silent', () => {
    const r = runHook(SESSION_START_SH, { ST_AGENT: 'bob' });
    expect(r.exitCode).not.toBe(0);
    expect(r.stdout).toBe('');
    expect(r.stderr).toContain('ST_ROOT not set');
  });

  it('missing ST_AGENT → exit non-zero, stderr message, stdout silent', () => {
    const r = runHook(SESSION_START_SH, { ST_ROOT: stRoot });
    expect(r.exitCode).not.toBe(0);
    expect(r.stdout).toBe('');
    expect(r.stderr).toContain('ST_AGENT not set');
  });

  it('files without `from:` still appear with from=unknown', () => {
    writeFileSync(
      join(stRoot, 'bob', 'inbox', '1714826789010-aaaaaa.md'),
      'no fence here, just words\n'
    );
    const r = runHook(SESSION_START_SH, {
      ST_ROOT: stRoot,
      ST_AGENT: 'bob',
    });
    expect(r.exitCode).toBe(0);
    const payload = JSON.parse(r.stdout) as { additionalContext: string };
    expect(payload.additionalContext).toContain('1714826789010-aaaaaa.md');
    expect(payload.additionalContext).toContain('unknown');
  });

  // brief-024 parity: codex agents restore working-state (context/now.md)
  // on cold-boot, same as the claude session-start hook.
  it('fresh now.md is injected as a <context> block before the inbox', () => {
    mkdirSync(join(stRoot, 'alice', 'context'), { recursive: true });
    writeFileSync(
      join(stRoot, 'alice', 'context', 'now.md'),
      '# now — alice\n\nTask: ship the widget; waiting on PR #7.\n'
    );
    plant(
      'alice',
      '1714826789010-aaaaaa.md',
      { from: 'bob', subject: 'deploy question' },
      'body'
    );
    const r = runHook(SESSION_START_SH, { ST_ROOT: stRoot, ST_AGENT: 'alice' });
    expect(r.exitCode).toBe(0);
    const payload = JSON.parse(r.stdout) as { additionalContext: string };
    expect(payload.additionalContext).toContain(
      '<context source="st/context/now.md" agent="alice">'
    );
    expect(payload.additionalContext).toContain('Task: ship the widget');
    expect(payload.additionalContext).toContain('</context>');
    expect(payload.additionalContext).toContain('## st inbox (1 unread)');
    // now.md block comes before the inbox snapshot.
    expect(payload.additionalContext.indexOf('</context>')).toBeLessThan(
      payload.additionalContext.indexOf('## st inbox')
    );
  });

  it('fresh now.md with an EMPTY inbox still emits (the parity fix)', () => {
    mkdirSync(join(stRoot, 'alice', 'context'), { recursive: true });
    writeFileSync(
      join(stRoot, 'alice', 'context', 'now.md'),
      '# now — alice\nstill working.\n'
    );
    const r = runHook(SESSION_START_SH, { ST_ROOT: stRoot, ST_AGENT: 'alice' });
    expect(r.exitCode).toBe(0);
    const payload = JSON.parse(r.stdout) as { additionalContext: string };
    expect(payload.additionalContext).toContain(
      '<context source="st/context/now.md" agent="alice">'
    );
    expect(payload.additionalContext).toContain('still working.');
    expect(payload.additionalContext).not.toContain('## st inbox');
  });

  it('stale now.md (ST_REHYDRATE_STALE_S=0) + empty inbox → silent, exit 0', () => {
    mkdirSync(join(stRoot, 'alice', 'context'), { recursive: true });
    writeFileSync(
      join(stRoot, 'alice', 'context', 'now.md'),
      '# now — alice\nold.\n'
    );
    const r = runHook(SESSION_START_SH, {
      ST_ROOT: stRoot,
      ST_AGENT: 'alice',
      ST_REHYDRATE_STALE_S: '0',
    });
    expect(r.exitCode).toBe(0);
    expect(r.stdout.trim()).toBe('');
  });
});

// ─── stop.sh ───────────────────────────────────────────────────────────

describe.skipIf(!HAS_JQ)('codex hooks — stop.sh', () => {
  it('first run with messages → emits payload; second run immediately → silent', () => {
    plant('bob', '1714826789010-aaaaaa.md', { from: 'alice' }, 'msg');

    const env = {
      ST_ROOT: stRoot,
      ST_AGENT: 'bob',
      XDG_STATE_HOME: stateHome,
    };

    const first = runHook(STOP_SH, env);
    expect(first.exitCode).toBe(0);
    const payload = JSON.parse(first.stdout) as {
      additionalContext: string;
      continue: boolean;
    };
    expect(payload.continue).toBe(true);
    expect(payload.additionalContext).toContain('1 new since last check');
    expect(payload.additionalContext).toContain('1714826789010-aaaaaa.md');

    // No new files between the two runs. Existing file's filename ts
    // is < last-checked, so st ls --since returns empty.
    const second = runHook(STOP_SH, env);
    expect(second.exitCode).toBe(0);
    expect(second.stdout).toBe('');
  });

  it('creates and updates state file at $XDG_STATE_HOME/st-codex-hooks/last-checked.txt', () => {
    const stateFile = join(
      stateHome,
      'st-codex-hooks',
      'last-checked.txt'
    );
    expect(existsSync(stateFile)).toBe(false);

    runHook(STOP_SH, {
      ST_ROOT: stRoot,
      ST_AGENT: 'bob',
      XDG_STATE_HOME: stateHome,
    });

    expect(existsSync(stateFile)).toBe(true);
    const first = readFileSync(stateFile, 'utf8').trim();
    expect(first).toMatch(/^[0-9]+$/);
    expect(Number(first)).toBeGreaterThan(0);

    // Run again; cursor advances even with an empty delta.
    // Sleep 5ms so the unix-ms timestamps are distinguishable.
    spawnSync('sleep', ['0.005']);
    runHook(STOP_SH, {
      ST_ROOT: stRoot,
      ST_AGENT: 'bob',
      XDG_STATE_HOME: stateHome,
    });
    const second = readFileSync(stateFile, 'utf8').trim();
    expect(Number(second)).toBeGreaterThanOrEqual(Number(first));
  });

  it('files arriving AFTER the cursor are reported on the next Stop', () => {
    const env = {
      ST_ROOT: stRoot,
      ST_AGENT: 'bob',
      XDG_STATE_HOME: stateHome,
    };

    // Establish the cursor against an empty inbox.
    runHook(STOP_SH, env);
    const stateFile = join(
      stateHome,
      'st-codex-hooks',
      'last-checked.txt'
    );
    const cursor = Number(readFileSync(stateFile, 'utf8').trim());

    // Plant a file with a filename ts > cursor so it shows up in the
    // next --since window.
    const fname = `${cursor + 1000}-cccccc.md`;
    plant('bob', fname, { from: 'alice', subject: 'hi' }, 'body');

    const r = runHook(STOP_SH, env);
    expect(r.exitCode).toBe(0);
    const payload = JSON.parse(r.stdout) as { additionalContext: string };
    expect(payload.additionalContext).toContain(fname);
  });

  it('missing ST_ROOT → exit non-zero, stderr message, stdout silent', () => {
    const r = runHook(STOP_SH, {
      ST_AGENT: 'bob',
      XDG_STATE_HOME: stateHome,
    });
    expect(r.exitCode).not.toBe(0);
    expect(r.stdout).toBe('');
    expect(r.stderr).toContain('ST_ROOT not set');
  });
});

// Mark ST_BIN as used (the helper resolution above is purely
// pathwork; this silences unused-import warnings if the constant is
// referenced only via PATH-prepending).
void ST_BIN;
