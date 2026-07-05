// commands/sync.ts — bidirectional rsync + sweep, per LAYOUT "What sync looks like".
//
// Surface (mirror of lib/cmd_sync.sh):
//   coord sync push <peer>      sweep, rsync push, sweep
//   coord sync pull <peer>      sweep, rsync pull, sweep
//   coord sync sweep            local-only sweep (verbose)
//   coord sync push --all       fan out push over peers.yaml
//   coord sync pull --all       fan out pull (recommended conservative cron)
//   coord sync --all            push then pull, every peer (aggressive)
//
// The actual rsync invocation is injected via `deps.runRsync` so unit
// tests can mock it. The default uses `child_process.spawnSync('rsync', ...)`.

import { spawnSync } from 'node:child_process';
import {
  existsSync,
  mkdirSync,
  readFileSync,
} from 'node:fs';
import { join } from 'node:path';

import { pluralize, sweep } from '../common.ts';
import {
  PeersConfigInvalidError,
  PeersConfigMissingError,
  SyncFailedError,
} from '../errors.ts';

// ─── Types ──────────────────────────────────────────────────────────────

export interface RsyncResult {
  status: number;
  stderr?: string;
}

export interface SyncDeps {
  /** Run `rsync -a <args...>`. Defaults to a real spawnSync invocation. */
  runRsync?: (args: string[]) => RsyncResult;
  /** Ensure a directory exists (used by the local: peer resolver). */
  ensureDir?: (path: string) => void;
  /** Banner emitter (defaults to stderr). */
  bannerSink?: (line: string) => void;
}

export interface SyncContext {
  coordRoot: string;
  coordConfig: string;
  deps?: SyncDeps;
}

// ─── peer resolution ────────────────────────────────────────────────────

/**
 * Resolve a peer spec to the `<rsync_root>` argument rsync will receive.
 * The trailing slash on the result is intentional — rsync treats it as
 * "contents of the dir" rather than "the dir itself."
 */
export function resolvePeer(spec: string, ctx: SyncContext): string {
  if (spec.startsWith('local:')) {
    const p = spec.slice('local:'.length);
    if (p.length === 0) {
      throw new PeersConfigInvalidError(spec, 'local: peer requires a path');
    }
    const ensureDir = ctx.deps?.ensureDir ?? defaultEnsureDir;
    ensureDir(p);
    return `${stripTrailingSlash(p)}/`;
  }
  if (spec.includes(':')) {
    return `${stripTrailingSlash(spec)}/`;
  }
  // Bare token: try peers.yaml alias first; else fall back to bare hostname.
  const aliased = lookupPeerAlias(spec, ctx.coordConfig);
  if (aliased !== undefined) {
    return resolvePeer(aliased, ctx);
  }
  return `${spec}:.local/state/coord/`;
}

function stripTrailingSlash(s: string): string {
  return s.endsWith('/') ? s.slice(0, -1) : s;
}

function defaultEnsureDir(p: string): void {
  mkdirSync(p, { recursive: true });
}

// ─── peers.yaml parsing ─────────────────────────────────────────────────

/**
 * Parse the simple `name: spec` line format used by peers.yaml.
 * - Lines starting with `#` are comments.
 * - Whitespace around name and value is trimmed.
 * - First colon splits name from value.
 * - Empty / unmappable lines are silently dropped.
 *
 * Returns an array preserving file order. Duplicate names → the last one
 * wins (matches the awk: it would emit each value, but `_lookup_peer_alias`
 * exits on first match).
 */
export function parsePeersYaml(text: string): { name: string; spec: string }[] {
  const out: { name: string; spec: string }[] = [];
  for (const rawLine of text.split('\n')) {
    const line = rawLine;
    if (/^[ \t]*#/.test(line)) continue;
    if (/^[ \t]*$/.test(line)) continue;
    const idx = line.indexOf(':');
    if (idx < 0) continue;
    const name = line.slice(0, idx).trim();
    const spec = line.slice(idx + 1).trim();
    if (name === '' || spec === '') continue;
    out.push({ name, spec });
  }
  return out;
}

function readPeersYaml(coordConfig: string): { name: string; spec: string }[] {
  const cfg = join(coordConfig, 'peers.yaml');
  if (!existsSync(cfg)) {
    throw new PeersConfigMissingError(cfg);
  }
  const peers = parsePeersYaml(readFileSync(cfg, 'utf8'));
  if (peers.length === 0) {
    throw new PeersConfigInvalidError(cfg, 'no peers found in');
  }
  return peers;
}

function lookupPeerAlias(
  name: string,
  coordConfig: string
): string | undefined {
  const cfg = join(coordConfig, 'peers.yaml');
  if (!existsSync(cfg)) return undefined;
  const peers = parsePeersYaml(readFileSync(cfg, 'utf8'));
  for (const p of peers) {
    if (p.name === name) return p.spec;
  }
  return undefined;
}

// ─── rsync invocation ───────────────────────────────────────────────────

function defaultRunRsync(args: string[]): RsyncResult {
  const r = spawnSync('rsync', ['-a', ...args], {
    stdio: ['inherit', 'inherit', 'pipe'],
    encoding: 'utf8',
  });
  return {
    status: r.status ?? -1,
    stderr: typeof r.stderr === 'string' ? r.stderr : undefined,
  };
}

// ─── sweep entry point (verbose) ────────────────────────────────────────

export function cmdSyncSweep(ctx: SyncContext): { removed: number; summary: string } {
  const r = sweep(ctx.coordRoot);
  const summary =
    r.removed > 0
      ? `# sweep: removed ${r.removed} redundant inbox ${pluralize(r.removed, 'file', 'files')}`
      : '';
  return { removed: r.removed, summary };
}

// ─── push / pull ────────────────────────────────────────────────────────

export interface PushPullResult {
  rsyncRoot: string;
  removedBefore: number;
  removedAfter: number;
}

export function cmdSyncPush(
  peerSpec: string,
  ctx: SyncContext
): PushPullResult {
  if (!existsSync(ctx.coordRoot)) {
    throw new PeersConfigInvalidError(
      ctx.coordRoot,
      'no COORD_ROOT to push from'
    );
  }
  const banner = ctx.deps?.bannerSink ?? defaultBanner;
  const rsyncRoot = resolvePeer(peerSpec, ctx);

  const before = sweep(ctx.coordRoot).removed;
  banner(`# push: ${ctx.coordRoot}/ -> ${rsyncRoot}`);
  const runRsync = ctx.deps?.runRsync ?? defaultRunRsync;
  const r = runRsync([`${ctx.coordRoot}/`, rsyncRoot]);
  if (r.status !== 0) {
    if (r.stderr) banner(r.stderr.trimEnd());
    throw new SyncFailedError(
      'push',
      r.status,
      r.stderr,
      `rsync push failed: ${ctx.coordRoot}/ -> ${rsyncRoot}`
    );
  }
  const after = sweep(ctx.coordRoot).removed;
  return { rsyncRoot, removedBefore: before, removedAfter: after };
}

export function cmdSyncPull(
  peerSpec: string,
  ctx: SyncContext
): PushPullResult {
  const banner = ctx.deps?.bannerSink ?? defaultBanner;
  const rsyncRoot = resolvePeer(peerSpec, ctx);
  mkdirSync(ctx.coordRoot, { recursive: true });

  const before = sweep(ctx.coordRoot).removed;
  banner(`# pull: ${rsyncRoot} -> ${ctx.coordRoot}/`);
  const runRsync = ctx.deps?.runRsync ?? defaultRunRsync;
  const r = runRsync([rsyncRoot, `${ctx.coordRoot}/`]);
  if (r.status !== 0) {
    if (r.stderr) banner(r.stderr.trimEnd());
    throw new SyncFailedError(
      'pull',
      r.status,
      r.stderr,
      `rsync pull failed: ${rsyncRoot} -> ${ctx.coordRoot}/`
    );
  }
  const after = sweep(ctx.coordRoot).removed;
  return { rsyncRoot, removedBefore: before, removedAfter: after };
}

// ─── --all fan-outs ─────────────────────────────────────────────────────

export interface FanOutResult {
  successes: string[];
  failures: { peer: string; error: string }[];
}

export function cmdSyncAllPush(ctx: SyncContext): FanOutResult {
  return fanOut('push', ctx, (spec) => cmdSyncPush(spec, ctx));
}

export function cmdSyncAllPull(ctx: SyncContext): FanOutResult {
  return fanOut('pull', ctx, (spec) => cmdSyncPull(spec, ctx));
}

/** push then pull for each peer (the aggressive `coord sync --all`). */
export function cmdSyncAll(ctx: SyncContext): FanOutResult {
  const banner = ctx.deps?.bannerSink ?? defaultBanner;
  const peers = readPeersYaml(ctx.coordConfig);
  const successes: string[] = [];
  const failures: { peer: string; error: string }[] = [];
  for (const { spec } of peers) {
    banner(`# === peer: ${spec} ===`);
    let pushOk = false;
    try {
      cmdSyncPush(spec, ctx);
      pushOk = true;
    } catch (err) {
      failures.push({ peer: spec, error: errMsg(err) });
    }
    let pullOk = false;
    try {
      cmdSyncPull(spec, ctx);
      pullOk = true;
    } catch (err) {
      failures.push({ peer: spec, error: errMsg(err) });
    }
    if (pushOk && pullOk) successes.push(spec);
  }
  return { successes, failures };
}

function fanOut(
  label: string,
  ctx: SyncContext,
  action: (spec: string) => unknown
): FanOutResult {
  const banner = ctx.deps?.bannerSink ?? defaultBanner;
  const peers = readPeersYaml(ctx.coordConfig);
  const successes: string[] = [];
  const failures: { peer: string; error: string }[] = [];
  for (const { spec } of peers) {
    banner(`# === ${label} peer: ${spec} ===`);
    try {
      action(spec);
      successes.push(spec);
    } catch (err) {
      failures.push({ peer: spec, error: errMsg(err) });
    }
  }
  return { successes, failures };
}

function errMsg(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

function defaultBanner(line: string): void {
  process.stderr.write(`${line}\n`);
}

export {
  cmdSyncAll as cmdSyncAllCore,
  cmdSyncAllPull as cmdSyncAllPullCore,
  cmdSyncAllPush as cmdSyncAllPushCore,
  cmdSyncPull as cmdSyncPullCore,
  cmdSyncPush as cmdSyncPushCore,
  cmdSyncSweep as cmdSyncSweepCore,
};

// ─── CLI wrapper ────────────────────────────────────────────────────────

import { invokedName, type CliContext } from '../cli-context.ts';

export function cmdSyncCli(args: readonly string[], ctx: CliContext): number {
  let verb: string | undefined;
  let peer: string | undefined;
  let all = false;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '--all':
        all = true;
        break;
      case '-h':
      case '--help':
        {
          const name = invokedName(ctx.env);
          ctx.stderr(
            `usage: ${name} sync push|pull <peer>\n` +
              `       ${name} sync push|pull --all\n` +
              `       ${name} sync sweep\n` +
              `       ${name} sync --all\n`
          );
        }
        return 0;
      default:
        if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
        if (verb === undefined) verb = a;
        else if (peer === undefined) peer = a;
        else throw new Error(`unexpected arg: ${a}`);
    }
  }
  const sctx: SyncContext = {
    coordRoot: ctx.coordRoot,
    coordConfig: ctx.coordConfig,
  };
  if (all) {
    if (peer !== undefined) throw new Error('--all takes no <peer> argument');
    switch (verb) {
      case undefined:
        cmdSyncAll(sctx);
        return 0;
      case 'push':
        cmdSyncAllPush(sctx);
        return 0;
      case 'pull':
        cmdSyncAllPull(sctx);
        return 0;
      case 'sweep':
        throw new Error(
          '--all is not meaningful for sweep (sweep is local-only)'
        );
      default:
        throw new Error(`--all does not pair with verb: ${verb}`);
    }
  }
  switch (verb) {
    case 'push':
      if (peer === undefined) throw new Error('<peer> required for push');
      cmdSyncPush(peer, sctx);
      return 0;
    case 'pull':
      if (peer === undefined) throw new Error('<peer> required for pull');
      cmdSyncPull(peer, sctx);
      return 0;
    case 'sweep': {
      if (peer !== undefined) throw new Error('sweep takes no <peer> argument');
      const r = cmdSyncSweep(sctx);
      if (r.summary !== '') ctx.stderr(`${r.summary}\n`);
      return 0;
    }
    case undefined:
      throw new Error('no verb. Use push|pull|sweep|--all');
    default:
      throw new Error(`unknown verb: ${verb}`);
  }
}
