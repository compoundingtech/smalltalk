// commands/init.ts — `st init` verb.
//
// Writes (or merges) `.mcp.json` in a target directory so a Claude
// Code session in that repo will load the coord MCP server. Resolves
// the bin/coord path portably (via this module's location, then
// `which coord` on PATH) so the file never carries a hardcoded
// developer-machine path.
//
// Per brief-026: surgical addition only — leaves other mcpServers
// entries untouched; pure idempotent on a match; prompt-gated on a
// divergent existing entry (skip via --force).

import { spawnSync } from 'node:child_process';
import {
  existsSync,
  readFileSync,
  renameSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs';
import { randomBytes } from 'node:crypto';
import { dirname, isAbsolute, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { invokedName, type CliContext } from '../cli-context.ts';

// ─── Shape ──────────────────────────────────────────────────────────────

interface McpServerEntry {
  type?: string;
  command?: string;
  args?: readonly string[];
  env?: Record<string, string>;
  // Other host-specific keys (per-server) are preserved verbatim if
  // someone hand-edited them. We never mutate keys we don't own.
  [k: string]: unknown;
}

interface McpJsonShape {
  mcpServers?: Record<string, McpServerEntry>;
  [k: string]: unknown;
}

export type InitOutcome =
  | 'wrote-new'
  | 'merged-into-existing'
  | 'already-configured'
  | 'overwrote-divergent'
  | 'skipped-by-user'
  | 'printed-only';

export interface InitInput {
  /** Target directory. `.mcp.json` is written inside this dir. */
  dir: string;
  /** When true, write `args: ["mcp"]` (no `--channel`). Default: include `--channel`. */
  noChannel?: boolean;
  /** When true, print the would-be entry to stdout and exit; touch no disk. */
  print?: boolean;
  /** When true, overwrite a divergent existing entry without prompting. */
  force?: boolean;
  /** Test seam: override the resolved bin/coord path. */
  binPath?: string;
  /** Test seam: prompt response. When set, used instead of stdin /TTY. */
  promptAnswer?: 'y' | 'n';
}

export interface InitResult {
  outcome: InitOutcome;
  path: string;
  /** The coord entry that was (or would have been) written. */
  entry: McpServerEntry;
}

// ─── Bin path resolution ────────────────────────────────────────────────

/**
 * Resolve a portable path to the smalltalk shim to embed as the MCP
 * server command in a project's `.mcp.json`. Strategy:
 *   1. Walk up from this module's file location to find the
 *      package.json whose `name === "@myobie/coord"` (the package
 *      name still says coord until the npm publish flips it), then
 *      return `<package-root>/bin/st`. Falls back to
 *      `<package-root>/bin/coord` only when `bin/st` isn't present
 *      on this install (very old package tarball / hand-installed
 *      old checkout).
 *   2. Fall back to `which st` on PATH, then `which coord` for
 *      legacy PATH setups.
 *
 * Prefers `bin/st` because it's the post-cutover canonical name —
 * matches what `st init` writes as the `.mcp.json` server key (`st`)
 * and what `enabledMcpjsonServers` pins. The old `bin/coord` was
 * dual-aliased into the same target, so both work today, but pinning
 * to the canonical name means the day we drop the coord alias
 * doesn't break every existing `.mcp.json`. Function name kept as
 * `resolveStShimPath` for now — this is a callers-are-me-only API,
 * renaming is scope-creep on the fix.
 *
 * Throws if neither the checkout walk nor the PATH lookup produces
 * an existing shim. Brief-026 boundary: NEVER hardcode a developer-
 * machine absolute path.
 */
export function resolveStShimPath(): string {
  const here = fileURLToPath(import.meta.url);
  let dir = dirname(here);
  for (let i = 0; i < 16; i++) {
    const pkgPath = join(dir, 'package.json');
    if (existsSync(pkgPath)) {
      try {
        const raw = readFileSync(pkgPath, 'utf8');
        const parsed = JSON.parse(raw) as { name?: string };
        if (parsed.name === '@myobie/coord') {
          // Prefer post-cutover canonical bin/st; fall back to bin/coord
          // if bin/st isn't present (very old package or hand-install
          // predating the cutover).
          const stCandidate = join(dir, 'bin', 'st');
          if (existsSync(stCandidate) && statSync(stCandidate).isFile()) {
            return stCandidate;
          }
          const coordCandidate = join(dir, 'bin', 'coord');
          if (
            existsSync(coordCandidate) &&
            statSync(coordCandidate).isFile()
          ) {
            return coordCandidate;
          }
        }
      } catch {
        // Malformed package.json — ignore and keep walking.
      }
    }
    const parent = dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  // PATH fallback — prefer `st`, then `coord` for legacy PATH.
  for (const name of ['st', 'coord']) {
    try {
      const r = spawnSync('which', [name], { encoding: 'utf8' });
      if (r.status === 0 && typeof r.stdout === 'string') {
        const found = r.stdout.trim();
        if (found.length > 0 && existsSync(found)) return found;
      }
    } catch {
      // ignore
    }
  }
  throw new Error(
    'st init: could not resolve a bin/st path. Install @myobie/coord ' +
      '(via npm) or add `st` to your $PATH and retry.'
  );
}

// ─── Entry shape ────────────────────────────────────────────────────────

function buildCoordEntry(
  binPath: string,
  noChannel: boolean
): McpServerEntry {
  return {
    type: 'stdio',
    command: binPath,
    args: noChannel ? ['mcp'] : ['mcp', '--channel'],
    env: {},
  };
}

/** True if two coord entries are byte-equivalent for our merge purposes. */
function entryMatches(a: McpServerEntry, b: McpServerEntry): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

// ─── Core ───────────────────────────────────────────────────────────────

export async function cmdInit(
  input: InitInput,
  ctx: CliContext
): Promise<InitResult> {
  const binPath =
    input.binPath !== undefined ? input.binPath : resolveStShimPath();
  const entry = buildCoordEntry(binPath, input.noChannel === true);
  const targetDir = isAbsolute(input.dir) ? input.dir : resolve(input.dir);
  const path = join(targetDir, '.mcp.json');

  if (input.print === true) {
    // Emit just the st entry (under a top-level mcpServers wrapper)
    // so the user can paste it manually if they want to. Key is `st`
    // to match the post-cutover on-disk naming; the MCP server itself
    // still responds to both `coord` and `st` names, so this key just
    // has to be internally consistent with `enabledMcpjsonServers`
    // in settings.local.json.
    const preview: McpJsonShape = { mcpServers: { st: entry } };
    ctx.stdout(`${JSON.stringify(preview, null, 2)}\n`);
    return { outcome: 'printed-only', path, entry };
  }

  if (!existsSync(targetDir) || !statSync(targetDir).isDirectory()) {
    throw new Error(`st init: target directory does not exist: ${targetDir}`);
  }

  let existing: McpJsonShape = {};
  let fileExisted = false;
  if (existsSync(path)) {
    fileExisted = true;
    let raw: string;
    try {
      raw = readFileSync(path, 'utf8');
    } catch (err) {
      throw new Error(
        `st init: could not read ${path}: ${(err as Error).message}`
      );
    }
    try {
      const parsed = JSON.parse(raw);
      if (
        typeof parsed !== 'object' ||
        parsed === null ||
        Array.isArray(parsed)
      ) {
        throw new Error(`top-level value is not an object`);
      }
      existing = parsed as McpJsonShape;
    } catch (err) {
      throw new Error(
        `st init: ${path} is not valid JSON: ${(err as Error).message}. Refusing to overwrite.`
      );
    }
  }

  const servers: Record<string, McpServerEntry> =
    typeof existing.mcpServers === 'object' &&
    existing.mcpServers !== null &&
    !Array.isArray(existing.mcpServers)
      ? { ...existing.mcpServers }
      : {};

  // Look up the existing entry under `st`. Post-coord-cutover the
  // legacy `coord` key is no longer read or written.
  const prior = servers.st;
  let outcome: InitOutcome;
  if (prior !== undefined && entryMatches(prior, entry)) {
    outcome = 'already-configured';
    ctx.stderr(
      `st init: ${path} already has matching st entry — no changes.\n`
    );
    return { outcome, path, entry };
  }

  if (prior !== undefined && !entryMatches(prior, entry)) {
    // Divergent. --force overrides; otherwise prompt.
    let overwrite = input.force === true;
    if (!overwrite) {
      const answer = await promptYesNo(
        ctx,
        input.promptAnswer,
        `st init: ${path} has a different st entry. Overwrite? [y/N] `
      );
      overwrite = answer;
    }
    if (!overwrite) {
      ctx.stderr(`st init: skipped — existing st entry preserved.\n`);
      return { outcome: 'skipped-by-user', path, entry: prior };
    }
    outcome = 'overwrote-divergent';
  } else if (fileExisted) {
    outcome = 'merged-into-existing';
  } else {
    outcome = 'wrote-new';
  }

  servers.st = entry;
  const next: McpJsonShape = { ...existing, mcpServers: servers };
  atomicWriteJson(path, next);

  const summary =
    outcome === 'wrote-new'
      ? `st init: wrote ${path}\n`
      : outcome === 'merged-into-existing'
      ? `st init: added st entry to existing ${path}\n`
      : `st init: overwrote divergent st entry in ${path}\n`;
  ctx.stderr(summary);

  return { outcome, path, entry };
}

function atomicWriteJson(path: string, value: unknown): void {
  const dir = dirname(path);
  const tmp = join(
    dir,
    `.mcp.json.tmp-${process.pid}-${randomBytes(3).toString('hex')}`
  );
  const content = `${JSON.stringify(value, null, 2)}\n`;
  writeFileSync(tmp, content);
  try {
    renameSync(tmp, path);
  } catch (err) {
    // Best-effort cleanup of the tmp file on rename failure.
    try {
      unlinkSync(tmp);
    } catch {
      // ignore
    }
    throw err;
  }
}

async function promptYesNo(
  ctx: CliContext,
  injected: 'y' | 'n' | undefined,
  prompt: string
): Promise<boolean> {
  if (injected !== undefined) return injected === 'y';
  // Production path: real TTY → readline prompt. Otherwise read piped
  // stdin via ctx.readStdin (tests stub this).
  if (process.stdin.isTTY === true) {
    const readline = await import('node:readline/promises');
    const rl = readline.createInterface({
      input: process.stdin,
      output: process.stderr,
    });
    try {
      const answer = await rl.question(prompt);
      return answer.trim().toLowerCase().startsWith('y');
    } finally {
      rl.close();
    }
  }
  ctx.stderr(prompt);
  const buf = await ctx.readStdin();
  return buf.toString('utf8').trim().toLowerCase().startsWith('y');
}

// ─── CLI wrapper ────────────────────────────────────────────────────────

function initHelp(name: string): string {
  return (
    `usage: ${name} init [<dir>] [--no-channel] [--print] [--force]\n\n` +
    '  Write or merge `.mcp.json` in <dir> (default: cwd) so a Claude\n' +
    '  Code session in that directory loads the smalltalk MCP server.\n\n' +
    '  --no-channel   Write args without `--channel` (pull-only host).\n' +
    '  --print        Print the JSON entry to stdout; do not write.\n' +
    '  --force        Overwrite a divergent existing entry without\n' +
    '                 prompting. (A byte-identical entry is always a\n' +
    '                 no-op; unrelated entries are preserved.)\n'
  );
}

export async function cmdInitCli(
  args: readonly string[],
  ctx: CliContext
): Promise<number> {
  let dir: string | undefined;
  let noChannel = false;
  let print = false;
  let force = false;
  for (const a of args) {
    switch (a) {
      case '--no-channel':
        noChannel = true;
        break;
      case '--print':
        print = true;
        break;
      case '--force':
        force = true;
        break;
      case '-h':
      case '--help':
        ctx.stderr(initHelp(invokedName(ctx.env)));
        return 0;
      default:
        if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
        if (dir === undefined) dir = a;
        else throw new Error(`unexpected positional arg: ${a}`);
    }
  }
  await cmdInit(
    {
      dir: dir ?? process.cwd(),
      noChannel,
      print,
      force,
    },
    ctx
  );
  return 0;
}
