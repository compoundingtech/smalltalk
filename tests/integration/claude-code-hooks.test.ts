// tests/integration/claude-code-hooks.test.ts — Claude Code hook
// scripts at examples/claude-code/hooks/{session-start,stop-failure}.sh.
//
// Both are bash scripts. The stop-failure script branches by
// `error_type` and shells out to `coord` for status changes and
// message sends; we shim `coord` so the tests assert which
// invocations the script would have made without touching real coord
// state. Skipped on hosts without `jq` on PATH (stop-failure.sh uses
// jq to parse the envelope).

import { spawnSync } from 'node:child_process';
import {
  chmodSync,
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
const SESSION_START_SH = join(
  REPO_ROOT,
  'examples',
  'claude-code',
  'hooks',
  'session-start.sh'
);
const PRE_COMPACT_SH = join(
  REPO_ROOT,
  'examples',
  'claude-code',
  'hooks',
  'pre-compact.sh'
);
const STOP_FAILURE_SH = join(
  REPO_ROOT,
  'examples',
  'claude-code',
  'hooks',
  'stop-failure.sh'
);

function jqAvailable(): boolean {
  return spawnSync('jq', ['--version'], { stdio: 'ignore' }).status === 0;
}
const HAS_JQ = jqAvailable();

let scratch: string;
let shimDir: string;
let shimLog: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-it-claude-'));
  shimDir = join(scratch, 'bin');
  shimLog = join(scratch, 'coord-shim.log');
  mkdirSync(shimDir, { recursive: true });

  // Plant a tiny `coord` shim that records every invocation's argv
  // (one line per call, NUL-separated args within a line so bodies
  // with spaces survive). The hook script will resolve `coord` to
  // this file because we prepend shimDir to PATH below.
  const shimPath = join(shimDir, 'coord');
  writeFileSync(
    shimPath,
    [
      '#!/bin/bash',
      '# Test shim — records argv to $COORD_SHIM_LOG, exit 0.',
      // Use printf with \0 between args so we can split unambiguously
      // even when a body argument contains spaces, quotes, or newlines.
      'for arg in "$@"; do printf "%s\\0" "$arg" >> "$COORD_SHIM_LOG"; done',
      'printf "\\n" >> "$COORD_SHIM_LOG"',
      'exit 0',
      '',
    ].join('\n')
  );
  chmodSync(shimPath, 0o755);
});

afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

interface RunResult {
  stdout: string;
  stderr: string;
  exitCode: number;
  calls: string[][]; // each invocation = list of argv strings
}

function readShimCalls(): string[][] {
  if (!existsSync(shimLog)) return [];
  const raw = readFileSync(shimLog, 'utf8');
  // Each line is one invocation; within a line, args are NUL-separated
  // with a trailing NUL before the newline.
  return raw
    .split('\n')
    .filter((line) => line.length > 0)
    .map((line) => line.split('\0').filter((arg) => arg.length > 0));
}

function runStopFailure(
  envelope: Record<string, unknown> | string,
  env: NodeJS.ProcessEnv = {}
): RunResult {
  const path = `${shimDir}:${process.env.PATH ?? ''}`;
  const fullEnv: NodeJS.ProcessEnv = {
    PATH: path,
    HOME: process.env.HOME,
    COORD_SHIM_LOG: shimLog,
    ...env,
  };
  const input =
    typeof envelope === 'string' ? envelope : JSON.stringify(envelope);
  const r = spawnSync('bash', [STOP_FAILURE_SH], {
    env: fullEnv,
    input,
    encoding: 'utf8',
    timeout: 15_000,
  });
  return {
    stdout: typeof r.stdout === 'string' ? r.stdout : '',
    stderr: typeof r.stderr === 'string' ? r.stderr : '',
    exitCode: r.status ?? -1,
    calls: readShimCalls(),
  };
}

// ─── session-start.sh ──────────────────────────────────────────────────

interface SessionStartRun {
  status: number;
  stderr: string;
  stdout: string;
}

function runSessionStart(env: NodeJS.ProcessEnv): SessionStartRun {
  const r = spawnSync('bash', [SESSION_START_SH], {
    encoding: 'utf8',
    timeout: 5_000,
    env: {
      // Purge parent env of the smalltalk identity vars — a
      // developer's shell running the tests could otherwise leak
      // ST_AGENT / COORD_IDENTITY and steer the hook against a real
      // ~/.local/state path. All identity is per-test.
      PATH: process.env.PATH,
      ...env,
    },
  });
  return {
    status: r.status ?? -1,
    stderr: typeof r.stderr === 'string' ? r.stderr : '',
    stdout: typeof r.stdout === 'string' ? r.stdout : '',
  };
}

// Set a file's mtime to (now - ageSeconds). Works on BSD stat (darwin)
// and GNU stat (linux) transparently — `touch -t` is portable enough
// for our purposes at second granularity, which is what the hook's
// mtime-based staleness check operates on.
function ageFile(path: string, ageSeconds: number): void {
  const d = new Date(Date.now() - ageSeconds * 1000);
  const yyyy = d.getFullYear().toString();
  const mm = (d.getMonth() + 1).toString().padStart(2, '0');
  const dd = d.getDate().toString().padStart(2, '0');
  const hh = d.getHours().toString().padStart(2, '0');
  const mi = d.getMinutes().toString().padStart(2, '0');
  const r = spawnSync(
    'touch',
    ['-t', `${yyyy}${mm}${dd}${hh}${mi}`, path],
    { encoding: 'utf8', timeout: 5_000 }
  );
  if (r.status !== 0) {
    throw new Error(
      `touch failed: status=${r.status}, stderr=${r.stderr ?? ''}`
    );
  }
}

describe('claude-code hooks — session-start.sh (bare)', () => {
  it('exits 2 and emits the boot-ritual reminder even with no identity', () => {
    const r = runSessionStart({});
    expect(r.status).toBe(2);
    expect(r.stderr).toContain('boot ritual');
    expect(r.stderr).toContain('set status to available');
    expect(r.stderr).toContain('drain inbox');
    expect(r.stderr).not.toContain('<context');
    expect(r.stdout).toBe('');
  });
});

// ─── session-start.sh — brief-024 boot-rehydrate ─────────────────────────
//
// The hook now inspects $COORD_ROOT/<identity>/context/now.md and, when
// fresh, injects it as a <context> block before the boot-ritual line.
// These tests exercise all four branches:
//   1. no identity → no injection, ritual only (regression guard).
//   2. identity but no now.md → no injection, ritual only.
//   3. identity + fresh now.md → injection + ritual.
//   4. identity + stale now.md (>staleness threshold) → no injection.

describe('claude-code hooks — session-start.sh boot-rehydrate (brief-024)', () => {
  it('identity set but no context/ → ritual only, no <context> block', () => {
    mkdirSync(join(scratch, 'alice', 'inbox'), { recursive: true });
    mkdirSync(join(scratch, 'alice', 'archive'), { recursive: true });
    const r = runSessionStart({
      COORD_ROOT: scratch,
      COORD_IDENTITY: 'alice',
    });
    expect(r.status).toBe(2);
    expect(r.stderr).not.toContain('<context');
    expect(r.stderr).toContain('boot ritual');
  });

  it('fresh now.md → injects as <context> block with identity attribute + agent name', () => {
    mkdirSync(join(scratch, 'alice', 'context'), { recursive: true });
    writeFileSync(
      join(scratch, 'alice', 'context', 'now.md'),
      '# now\ncurrent task: brief-024 hook legs\n'
    );
    const r = runSessionStart({
      COORD_ROOT: scratch,
      COORD_IDENTITY: 'alice',
    });
    expect(r.status).toBe(2);
    expect(r.stderr).toContain(
      '<context source="coord/context/now.md" agent="alice">'
    );
    expect(r.stderr).toContain('current task: brief-024 hook legs');
    expect(r.stderr).toContain('</context>');
    // Ritual reminder is still there — the injection is additive.
    expect(r.stderr).toContain('boot ritual');
    // The <context> block must precede the ritual line so the model
    // reads state before being told to drain the inbox.
    const ctxIdx = r.stderr.indexOf('<context');
    const ritualIdx = r.stderr.indexOf('boot ritual');
    expect(ctxIdx).toBeGreaterThanOrEqual(0);
    expect(ritualIdx).toBeGreaterThan(ctxIdx);
  });

  it('injected block is well-formed even when now.md has no trailing newline', () => {
    mkdirSync(join(scratch, 'alice', 'context'), { recursive: true });
    // Deliberately omit trailing \n — the hook must add one so </context>
    // lands on its own line.
    writeFileSync(
      join(scratch, 'alice', 'context', 'now.md'),
      'no-trailing-newline'
    );
    const r = runSessionStart({
      COORD_ROOT: scratch,
      COORD_IDENTITY: 'alice',
    });
    expect(r.stderr).toMatch(/no-trailing-newline\n<\/context>/);
  });

  it('stale now.md (>24h old) → no injection, ritual only', () => {
    mkdirSync(join(scratch, 'alice', 'context'), { recursive: true });
    const nowPath = join(scratch, 'alice', 'context', 'now.md');
    writeFileSync(nowPath, '# now\ntwo-day-old state\n');
    // 2 days = 172_800s; well past the 24h (86_400s) default threshold.
    ageFile(nowPath, 172_800);
    const r = runSessionStart({
      COORD_ROOT: scratch,
      COORD_IDENTITY: 'alice',
    });
    expect(r.stderr).not.toContain('<context');
    expect(r.stderr).not.toContain('two-day-old state');
    expect(r.stderr).toContain('boot ritual');
  });

  it('$COORD_REHYDRATE_STALE_S overrides the staleness threshold', () => {
    mkdirSync(join(scratch, 'alice', 'context'), { recursive: true });
    const nowPath = join(scratch, 'alice', 'context', 'now.md');
    writeFileSync(nowPath, '# now\nninety-minute-old state\n');
    ageFile(nowPath, 5_400); // 90 minutes

    // With the default 24h threshold, 90 min is fresh → injects.
    const rDefault = runSessionStart({
      COORD_ROOT: scratch,
      COORD_IDENTITY: 'alice',
    });
    expect(rDefault.stderr).toContain('ninety-minute-old state');

    // With a 60-minute threshold, 90 min is stale → no injection.
    const rTight = runSessionStart({
      COORD_ROOT: scratch,
      COORD_IDENTITY: 'alice',
      COORD_REHYDRATE_STALE_S: '3600',
    });
    expect(rTight.stderr).not.toContain('ninety-minute-old state');
    expect(rTight.stderr).toContain('boot ritual');
  });

  it('honors the ST_AGENT / ST_IDENTITY fallback chain for the identity resolve', () => {
    // The hook's identity resolution mirrors coord's: ST_AGENT >
    // ST_IDENTITY > COORD_IDENTITY. Prove it by populating alice's
    // context under ST_AGENT and confirming injection.
    mkdirSync(join(scratch, 'alice', 'context'), { recursive: true });
    writeFileSync(
      join(scratch, 'alice', 'context', 'now.md'),
      'st-agent path\n'
    );
    const r = runSessionStart({
      COORD_ROOT: scratch,
      ST_AGENT: 'alice',
    });
    expect(r.stderr).toContain('st-agent path');
  });
});

// ─── pre-compact.sh — brief-024 flush ────────────────────────────────────

interface PreCompactRun {
  status: number;
  stderr: string;
  stdout: string;
  calls: string[][];
}

function runPreCompact(env: NodeJS.ProcessEnv): PreCompactRun {
  const path = `${shimDir}:${process.env.PATH ?? ''}`;
  const r = spawnSync('bash', [PRE_COMPACT_SH], {
    encoding: 'utf8',
    timeout: 5_000,
    env: {
      PATH: path,
      COORD_SHIM_LOG: shimLog,
      ...env,
    },
  });
  return {
    status: r.status ?? -1,
    stderr: typeof r.stderr === 'string' ? r.stderr : '',
    stdout: typeof r.stdout === 'string' ? r.stdout : '',
    calls: readShimCalls(),
  };
}

describe('claude-code hooks — pre-compact.sh (brief-024)', () => {
  it('exits 0 even without any identity env — never blocks compaction', () => {
    const r = runPreCompact({});
    // The prime directive.
    expect(r.status).toBe(0);
    expect(r.calls).toEqual([]);
    // stderr must be empty (the hook writes errors to a file, not
    // stderr, so Claude Code doesn't inject reminders on compaction).
    expect(r.stderr).toBe('');
  });

  it('with identity + no now.md → invokes `coord context write` (implicit identity, no positional)', () => {
    mkdirSync(join(scratch, 'alice', 'inbox'), { recursive: true });
    mkdirSync(join(scratch, 'alice', 'archive'), { recursive: true });
    const r = runPreCompact({
      COORD_ROOT: scratch,
      COORD_IDENTITY: 'alice',
    });
    expect(r.status).toBe(0);
    expect(r.calls.length).toBe(1);
    const call = r.calls[0]!;
    // Load-bearing: no positional identity argument. Passing "$identity"
    // positionally would trigger the anti-impersonation strict check
    // in resolveAgent and fail on brand-new agents. We rely on the
    // env-var fallback path (same chain the hook itself used to
    // derive $identity) so `context write` takes the implicit
    // lazy-create branch.
    expect(call).toEqual(['context', 'write']);
    // stderr silent — errors go to a file, never stderr.
    expect(r.stderr).toBe('');
  });

  it('with FRESH now.md (<5 min) → NO write; model already flushed', () => {
    mkdirSync(join(scratch, 'alice', 'context'), { recursive: true });
    writeFileSync(
      join(scratch, 'alice', 'context', 'now.md'),
      'model flushed this recently'
    );
    // Age 1 minute — well under the 5-minute default threshold.
    ageFile(join(scratch, 'alice', 'context', 'now.md'), 60);
    const r = runPreCompact({
      COORD_ROOT: scratch,
      COORD_IDENTITY: 'alice',
    });
    expect(r.status).toBe(0);
    // No `coord context write` — the model's fresh flush wins.
    expect(r.calls).toEqual([]);
  });

  it('with STALE now.md (>5 min) → writes stub via `coord context write`', () => {
    mkdirSync(join(scratch, 'alice', 'context'), { recursive: true });
    writeFileSync(
      join(scratch, 'alice', 'context', 'now.md'),
      'old state the model forgot to refresh'
    );
    // Age 10 minutes — well past the 5-minute default threshold.
    ageFile(join(scratch, 'alice', 'context', 'now.md'), 600);
    const r = runPreCompact({
      COORD_ROOT: scratch,
      COORD_IDENTITY: 'alice',
    });
    expect(r.status).toBe(0);
    expect(r.calls.length).toBe(1);
    expect(r.calls[0]).toEqual(['context', 'write']);
  });

  it('$COORD_PRECOMPACT_FRESH_S overrides the freshness threshold', () => {
    mkdirSync(join(scratch, 'alice', 'context'), { recursive: true });
    writeFileSync(
      join(scratch, 'alice', 'context', 'now.md'),
      '30-second-old flush'
    );
    ageFile(join(scratch, 'alice', 'context', 'now.md'), 30);

    // Default (300s): 30 s is fresh, skip.
    const rDefault = runPreCompact({
      COORD_ROOT: scratch,
      COORD_IDENTITY: 'alice',
    });
    expect(rDefault.calls).toEqual([]);

    // Tighten to 15s: 30 s is stale, must write.
    const rTight = runPreCompact({
      COORD_ROOT: scratch,
      COORD_IDENTITY: 'alice',
      COORD_PRECOMPACT_FRESH_S: '15',
    });
    expect(rTight.calls.length).toBe(1);
  });

  it('honors ST_AGENT identity chain (same as session-start.sh)', () => {
    mkdirSync(join(scratch, 'alice', 'inbox'), { recursive: true });
    mkdirSync(join(scratch, 'alice', 'archive'), { recursive: true });
    const r = runPreCompact({
      COORD_ROOT: scratch,
      ST_AGENT: 'alice',
    });
    expect(r.status).toBe(0);
    expect(r.calls.length).toBe(1);
    expect(r.calls[0]).toEqual(['context', 'write']);
  });

  it('when the shim exits nonzero, the hook still exits 0 (prime directive)', () => {
    // Replace the shim with a version that always fails.
    writeFileSync(
      join(shimDir, 'coord'),
      [
        '#!/bin/bash',
        '# Test shim — record args to $COORD_SHIM_LOG then FAIL.',
        'for arg in "$@"; do printf "%s\\0" "$arg" >> "$COORD_SHIM_LOG"; done',
        'printf "\\n" >> "$COORD_SHIM_LOG"',
        'echo "coord: simulated failure" >&2',
        'exit 1',
        '',
      ].join('\n')
    );
    chmodSync(join(shimDir, 'coord'), 0o755);
    const r = runPreCompact({
      COORD_ROOT: scratch,
      COORD_IDENTITY: 'alice',
    });
    // Load-bearing: the hook MUST exit 0 even when the underlying
    // write fails. Blocking compaction is worse than skipping a flush.
    expect(r.status).toBe(0);
    expect(r.calls.length).toBe(1);
    // Error text should not have reached the hook's stderr — the hook
    // redirects the write's stderr to a log file.
    expect(r.stderr).not.toContain('simulated failure');
  });
});

// ─── stop-failure.sh ───────────────────────────────────────────────────

describe.skipIf(!HAS_JQ)('claude-code hooks — stop-failure.sh', () => {
  // ── rate_limit: status=away only, no ding ────────────────────────────
  it('rate_limit → coord status away, no message send', () => {
    const r = runStopFailure(
      { error_type: 'rate_limit', session_id: 'abc' },
      { COORD_IDENTITY: 'bob' }
    );
    expect(r.exitCode).toBe(0);
    expect(r.calls).toEqual([['status', 'bob', '--set', 'away']]);
  });

  // ── server_error: status=away + ding ─────────────────────────────────
  it('server_error → status away + message to myobie (no priority high)', () => {
    const r = runStopFailure(
      { error_type: 'server_error' },
      { COORD_IDENTITY: 'bob' }
    );
    expect(r.exitCode).toBe(0);
    expect(r.calls.length).toBe(2);
    expect(r.calls[0]).toEqual(['status', 'bob', '--set', 'away']);
    const send = r.calls[1]!;
    expect(send[0]).toBe('message');
    expect(send[1]).toBe('send');
    expect(send[2]).toBe('myobie');
    expect(send).not.toContain('--priority');
    expect(send).toContain('--subject');
    const subject = send[send.indexOf('--subject') + 1]!;
    expect(subject).toContain('server_error');
    expect(subject).toContain('bob');
    expect(send).toContain('-m');
  });

  // ── authentication_failed: offline + priority high ───────────────────
  it('authentication_failed → status offline + priority-high ding', () => {
    const r = runStopFailure(
      { error_type: 'authentication_failed' },
      { COORD_IDENTITY: 'bob' }
    );
    expect(r.exitCode).toBe(0);
    expect(r.calls.length).toBe(2);
    expect(r.calls[0]).toEqual(['status', 'bob', '--set', 'offline']);
    const send = r.calls[1]!;
    expect(send.slice(0, 3)).toEqual(['message', 'send', 'myobie']);
    expect(send).toContain('--priority');
    expect(send[send.indexOf('--priority') + 1]).toBe('high');
    const subject = send[send.indexOf('--subject') + 1]!;
    expect(subject).toContain('auth failed');
    expect(subject).toContain('authentication_failed');
  });

  // ── oauth_org_not_allowed: same shape as authentication_failed ───────
  it('oauth_org_not_allowed → status offline + priority-high ding (auth-shape)', () => {
    const r = runStopFailure(
      { error_type: 'oauth_org_not_allowed' },
      { COORD_IDENTITY: 'bob' }
    );
    expect(r.exitCode).toBe(0);
    expect(r.calls.length).toBe(2);
    expect(r.calls[0]).toEqual(['status', 'bob', '--set', 'offline']);
    const send = r.calls[1]!;
    expect(send[send.indexOf('--priority') + 1]).toBe('high');
    const subject = send[send.indexOf('--subject') + 1]!;
    expect(subject).toContain('oauth_org_not_allowed');
  });

  // ── billing_error: offline + priority high + "billing" subject ───────
  it('billing_error → status offline + priority-high billing ding', () => {
    const r = runStopFailure(
      { error_type: 'billing_error' },
      { COORD_IDENTITY: 'bob' }
    );
    expect(r.exitCode).toBe(0);
    expect(r.calls.length).toBe(2);
    expect(r.calls[0]).toEqual(['status', 'bob', '--set', 'offline']);
    const send = r.calls[1]!;
    expect(send[send.indexOf('--priority') + 1]).toBe('high');
    const subject = send[send.indexOf('--subject') + 1]!;
    expect(subject).toContain('billing');
  });

  // ── programmer-error types: no coord calls at all ────────────────────
  for (const errType of [
    'max_output_tokens',
    'invalid_request',
    'model_not_found',
  ]) {
    it(`${errType} → no coord calls (programmer error, not infra)`, () => {
      const r = runStopFailure(
        { error_type: errType },
        { COORD_IDENTITY: 'bob' }
      );
      expect(r.exitCode).toBe(0);
      expect(r.calls).toEqual([]);
    });
  }

  // ── unknown: away + ding with error_type verbatim in subject + body ──
  it('unknown → status away + ding with error_type verbatim', () => {
    const r = runStopFailure(
      { error_type: 'unknown' },
      { COORD_IDENTITY: 'bob' }
    );
    expect(r.exitCode).toBe(0);
    expect(r.calls.length).toBe(2);
    expect(r.calls[0]).toEqual(['status', 'bob', '--set', 'away']);
    const send = r.calls[1]!;
    const subject = send[send.indexOf('--subject') + 1]!;
    const body = send[send.indexOf('-m') + 1]!;
    expect(subject).toContain('unknown');
    expect(body).toContain('error_type=unknown');
  });

  // ── novel error_type: catch-all path, verbatim in subject + body ─────
  it('novel error_type (not in the table) → catch-all: away + ding verbatim', () => {
    const r = runStopFailure(
      { error_type: 'overloaded_error' },
      { COORD_IDENTITY: 'bob' }
    );
    expect(r.exitCode).toBe(0);
    expect(r.calls.length).toBe(2);
    expect(r.calls[0]).toEqual(['status', 'bob', '--set', 'away']);
    const send = r.calls[1]!;
    const subject = send[send.indexOf('--subject') + 1]!;
    const body = send[send.indexOf('-m') + 1]!;
    expect(subject).toContain('overloaded_error');
    expect(body).toContain('error_type=overloaded_error');
  });

  // ── identity propagation: a different COORD_IDENTITY → in subject ────
  it('uses $COORD_IDENTITY in status target and subject', () => {
    const r = runStopFailure(
      { error_type: 'server_error' },
      { COORD_IDENTITY: 'coord-claude' }
    );
    expect(r.exitCode).toBe(0);
    expect(r.calls[0]).toEqual(['status', 'coord-claude', '--set', 'away']);
    const send = r.calls[1]!;
    const subject = send[send.indexOf('--subject') + 1]!;
    expect(subject).toContain('coord-claude');
  });

  // ── missing COORD_IDENTITY: silent exit 0, no coord calls ────────────
  it('missing COORD_IDENTITY → exit 0, no coord calls (silent)', () => {
    const r = runStopFailure({ error_type: 'rate_limit' }, {});
    expect(r.exitCode).toBe(0);
    expect(r.calls).toEqual([]);
  });

  // ── missing error_type field: treated as unknown ─────────────────────
  it('envelope without error_type → catch-all (away + ding)', () => {
    const r = runStopFailure(
      { session_id: 'abc' },
      { COORD_IDENTITY: 'bob' }
    );
    expect(r.exitCode).toBe(0);
    expect(r.calls.length).toBe(2);
    expect(r.calls[0]).toEqual(['status', 'bob', '--set', 'away']);
  });

  // ── malformed JSON stdin: treated as unknown, never throws ───────────
  it('non-JSON stdin → catch-all (defensive: never crash the hook)', () => {
    const r = runStopFailure('not json at all', { COORD_IDENTITY: 'bob' });
    expect(r.exitCode).toBe(0);
    expect(r.calls.length).toBe(2);
    expect(r.calls[0]).toEqual(['status', 'bob', '--set', 'away']);
  });
});
