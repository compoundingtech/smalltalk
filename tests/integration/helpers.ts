// tests/integration/helpers.ts — shared utilities for the real-bin/st suite.
//
// These helpers spawn the actual `bin/st` binary (which execs `node
// --experimental-strip-types src/cli.ts`) so the integration tests
// exercise argv parsing, stdin handling, exit codes, and real-rsync
// invocation end to end.

import { spawnSync, type SpawnSyncOptions } from 'node:child_process';
import {
  mkdirSync,
  mkdtempSync,
  rmSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { Session } from '@myobie/pty/testing';

const HERE = dirname(fileURLToPath(import.meta.url));
export const REPO_ROOT = resolve(HERE, '..', '..');
export const COORD_BIN = join(REPO_ROOT, 'bin', 'st');

// ─── Auto-cleanup of scratch dirs ───────────────────────────────────────

const cleanupTargets = new Set<string>();

function registerCleanup(path: string): void {
  cleanupTargets.add(path);
}

function exitCleanup(): void {
  for (const path of cleanupTargets) {
    try {
      rmSync(path, { recursive: true, force: true });
    } catch {
      // best effort
    }
  }
  cleanupTargets.clear();
}

process.on('exit', exitCleanup);
process.on('SIGINT', () => {
  exitCleanup();
  process.exit(130);
});
process.on('SIGTERM', () => {
  exitCleanup();
  process.exit(143);
});

/**
 * Creates a fresh scratch dir under `/tmp/coord-it-<random>/` and
 * returns its absolute path. The dir is auto-removed on process exit;
 * tests that want immediate cleanup can call `cleanupRoot(path)`.
 */
export function mkScratch(): string {
  const path = mkdtempSync(join(tmpdir(), 'coord-it-'));
  registerCleanup(path);
  return path;
}

/**
 * Creates a fresh `$ST_ROOT` subdirectory under a scratch dir.
 * Equivalent to mkScratch() + a child `coord/` folder so tests that
 * also want a scratch peer directory can do `mkScratch()` separately.
 */
export function mkRoot(): string {
  const scratch = mkScratch();
  const root = join(scratch, 'coord');
  mkdirSync(root, { recursive: true });
  return root;
}

export function cleanupRoot(path: string): void {
  try {
    rmSync(path, { recursive: true, force: true });
  } catch {
    // ignore
  }
  cleanupTargets.delete(path);
}

/**
 * Creates `<root>/<id>/{inbox,archive}/` so resolveIdentity passes for
 * `<id>` against this root.
 */
export function mkIdentity(root: string, id: string): void {
  mkdirSync(join(root, id, 'inbox'), { recursive: true });
  mkdirSync(join(root, id, 'archive'), { recursive: true });
}

// ─── runCoord (one-shot child_process) ──────────────────────────────────

export interface RunStOptions {
  /** $ST_ROOT for this invocation. */
  stRoot?: string;
  /** $ST_CONFIG for this invocation. */
  stConfig?: string;
  /** $ST_AGENT for this invocation. */
  coordIdentity?: string;
  /** Additional env variables (merged on top of the above). */
  env?: NodeJS.ProcessEnv;
  /** stdin content. Falsy = no stdin. */
  stdin?: string | Buffer;
  /** Working directory. Defaults to REPO_ROOT. */
  cwd?: string;
  /** Time budget for the command. Defaults to 30s. */
  timeoutMs?: number;
}

export interface RunCoordResult {
  stdout: string;
  stderr: string;
  exitCode: number;
}

/**
 * Spawns `bin/coord <args...>` synchronously and returns captured stdout,
 * stderr, and exit code. Uses spawnSync so tests stay simple — the only
 * commands that need streaming output are `st watch`, which uses
 * {@link runCoordPty} instead.
 */
export function runCoord(
  args: readonly string[],
  opts: RunStOptions = {}
): RunCoordResult {
  const env: NodeJS.ProcessEnv = {
    // Strip parent COORD_* vars so a misconfigured shell can't leak in.
    PATH: process.env.PATH,
    HOME: process.env.HOME,
    ...(opts.env ?? {}),
  };
  if (opts.stRoot !== undefined) env.ST_ROOT = opts.stRoot;
  if (opts.stConfig !== undefined) env.ST_CONFIG = opts.stConfig;
  if (opts.coordIdentity !== undefined) env.ST_AGENT = opts.coordIdentity;

  const spawnOpts: SpawnSyncOptions = {
    cwd: opts.cwd ?? REPO_ROOT,
    env,
    encoding: 'utf8',
    timeout: opts.timeoutMs ?? 30_000,
    // Default 1MB cap is too small for the 1MB-body roundtrip tests.
    maxBuffer: 16 * 1024 * 1024,
  };
  if (opts.stdin !== undefined) {
    spawnOpts.input = opts.stdin;
  }

  const r = spawnSync(COORD_BIN, [...args], spawnOpts);
  return {
    stdout: typeof r.stdout === 'string' ? r.stdout : '',
    stderr: typeof r.stderr === 'string' ? r.stderr : '',
    exitCode: r.status ?? -1,
  };
}

// ─── runCoordPty (real PTY via @myobie/pty/testing Session.spawn) ──────

export interface RunCoordPtyOptions {
  stRoot?: string;
  stConfig?: string;
  coordIdentity?: string;
  env?: Record<string, string>;
  rows?: number;
  cols?: number;
}

/**
 * Spawn `bin/coord <args...>` inside a PTY via `@myobie/pty/testing`.
 * Returns the {@link Session} so the test can `screenshot()`,
 * `waitForText()`, etc. **The caller MUST call `session.close()` in
 * an afterEach.**
 *
 * Note: Session.spawn merges its `env` opts on top of process.env. We
 * pass through PATH and HOME explicitly and clear COORD_* leakage by
 * setting empty values that {@link runCoord}'s contract avoids.
 */
export function runCoordPty(
  args: readonly string[],
  opts: RunCoordPtyOptions = {}
): Session {
  const env: Record<string, string> = {};
  if (opts.stRoot !== undefined) env.ST_ROOT = opts.stRoot;
  if (opts.stConfig !== undefined) env.ST_CONFIG = opts.stConfig;
  if (opts.coordIdentity !== undefined) env.ST_AGENT = opts.coordIdentity;
  Object.assign(env, opts.env ?? {});

  const spawnOpts: { rows?: number; cols?: number; env: Record<string, string> } = {
    env,
  };
  if (opts.rows !== undefined) spawnOpts.rows = opts.rows;
  if (opts.cols !== undefined) spawnOpts.cols = opts.cols;
  return Session.spawn(COORD_BIN, [...args], spawnOpts);
}

// ─── Filesystem assertion helpers ───────────────────────────────────────

import { existsSync, readFileSync, readdirSync } from 'node:fs';

export function listInbox(root: string, id: string): string[] {
  const dir = join(root, id, 'inbox');
  if (!existsSync(dir)) return [];
  return readdirSync(dir).sort();
}

export function listArchive(root: string, id: string): string[] {
  const dir = join(root, id, 'archive');
  if (!existsSync(dir)) return [];
  return readdirSync(dir).sort();
}

export function readMessageBody(
  root: string,
  id: string,
  filename: string,
  folder: 'inbox' | 'archive' = 'inbox'
): string {
  return readFileSync(join(root, id, folder, filename), 'utf8');
}

/** Asserts rsync is available on $PATH. Used by skipIf gating. */
export function rsyncAvailable(): boolean {
  const r = spawnSync('rsync', ['--version'], { stdio: 'ignore' });
  return r.status === 0;
}

/**
 * Like {@link rsyncAvailable}, but also requires a MODERN rsync. macOS ships
 * openrsync (Apple's reimplementation) as `/usr/bin/rsync`, which cannot do the
 * daemon-over-rsh + protect/risk `--delete` that `st sync fabric`'s remote
 * sweep relies on. Tests exercising remote deletion gate on this so they SKIP
 * (not FAIL) on a stock Mac — mirroring the tool's own runtime openrsync reject.
 */
export function modernRsyncAvailable(): boolean {
  const r = spawnSync('rsync', ['--version'], { encoding: 'utf8' });
  if (r.status !== 0) return false;
  const out = `${typeof r.stdout === 'string' ? r.stdout : ''}${typeof r.stderr === 'string' ? r.stderr : ''}`;
  return !/openrsync/i.test(out);
}
