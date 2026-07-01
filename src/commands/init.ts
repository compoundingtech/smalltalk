// commands/init.ts — `coord init` verb.
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

import type { CliContext } from '../cli-context.ts';

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
 * Resolve a portable path to `bin/coord`. Strategy:
 *   1. Walk up from this module's file location to find the
 *      package.json whose `name === "@myobie/coord"`, then return
 *      `<package-root>/bin/coord` (works under npm-install, `npm link`,
 *      or running directly out of a checkout — `import.meta.url` is
 *      the realpath of this file in all three modes).
 *   2. Fall back to `which coord` on PATH (for users who installed
 *      coord globally and want to point .mcp.json at the PATH lookup).
 *
 * Throws if neither path produces an existing bin/coord file. Brief-026
 * boundary: NEVER hardcode a developer-machine absolute path.
 */
export function resolveCoordBinPath(): string {
  const here = fileURLToPath(import.meta.url);
  let dir = dirname(here);
  for (let i = 0; i < 16; i++) {
    const pkgPath = join(dir, 'package.json');
    if (existsSync(pkgPath)) {
      try {
        const raw = readFileSync(pkgPath, 'utf8');
        const parsed = JSON.parse(raw) as { name?: string };
        if (parsed.name === '@myobie/coord') {
          const candidate = join(dir, 'bin', 'coord');
          if (existsSync(candidate) && statSync(candidate).isFile()) {
            return candidate;
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
  // PATH fallback.
  try {
    const r = spawnSync('which', ['coord'], { encoding: 'utf8' });
    if (r.status === 0 && typeof r.stdout === 'string') {
      const found = r.stdout.trim();
      if (found.length > 0 && existsSync(found)) return found;
    }
  } catch {
    // ignore
  }
  throw new Error(
    'coord init: could not resolve a bin/coord path. Install @myobie/coord ' +
      '(via npm) or add `coord` to your $PATH and retry.'
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
    input.binPath !== undefined ? input.binPath : resolveCoordBinPath();
  const entry = buildCoordEntry(binPath, input.noChannel === true);
  const targetDir = isAbsolute(input.dir) ? input.dir : resolve(input.dir);
  const path = join(targetDir, '.mcp.json');

  if (input.print === true) {
    // Emit just the coord entry (under a top-level mcpServers wrapper)
    // so the user can paste it manually if they want to.
    const preview: McpJsonShape = { mcpServers: { coord: entry } };
    ctx.stdout(`${JSON.stringify(preview, null, 2)}\n`);
    return { outcome: 'printed-only', path, entry };
  }

  if (!existsSync(targetDir) || !statSync(targetDir).isDirectory()) {
    throw new Error(`coord init: target directory does not exist: ${targetDir}`);
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
        `coord init: could not read ${path}: ${(err as Error).message}`
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
        `coord init: ${path} is not valid JSON: ${(err as Error).message}. Refusing to overwrite.`
      );
    }
  }

  const servers: Record<string, McpServerEntry> =
    typeof existing.mcpServers === 'object' &&
    existing.mcpServers !== null &&
    !Array.isArray(existing.mcpServers)
      ? { ...existing.mcpServers }
      : {};

  const prior = servers.coord;
  let outcome: InitOutcome;
  if (prior !== undefined && entryMatches(prior, entry)) {
    outcome = 'already-configured';
    ctx.stderr(`coord init: ${path} already has matching coord entry — no changes.\n`);
    return { outcome, path, entry };
  }

  if (prior !== undefined && !entryMatches(prior, entry)) {
    // Divergent. --force overrides; otherwise prompt.
    let overwrite = input.force === true;
    if (!overwrite) {
      const answer = await promptYesNo(
        ctx,
        input.promptAnswer,
        `coord init: ${path} has a different coord entry. Overwrite? [y/N] `
      );
      overwrite = answer;
    }
    if (!overwrite) {
      ctx.stderr(`coord init: skipped — existing coord entry preserved.\n`);
      return { outcome: 'skipped-by-user', path, entry: prior };
    }
    outcome = 'overwrote-divergent';
  } else if (fileExisted) {
    outcome = 'merged-into-existing';
  } else {
    outcome = 'wrote-new';
  }

  servers.coord = entry;
  const next: McpJsonShape = { ...existing, mcpServers: servers };
  atomicWriteJson(path, next);

  const summary =
    outcome === 'wrote-new'
      ? `coord init: wrote ${path}\n`
      : outcome === 'merged-into-existing'
      ? `coord init: added coord entry to existing ${path}\n`
      : `coord init: overwrote divergent coord entry in ${path}\n`;
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

const INIT_HELP =
  'usage: coord init [<dir>] [--no-channel] [--print] [--force]\n\n' +
  '  Write or merge `.mcp.json` in <dir> (default: cwd) so a Claude\n' +
  '  Code session in that directory loads the coord MCP server.\n\n' +
  '  --no-channel   Write args without `--channel` (pull-only host).\n' +
  '  --print        Print the JSON entry to stdout; do not write.\n' +
  '  --force        Overwrite a divergent existing coord entry without\n' +
  '                 prompting. (A byte-identical entry is always a\n' +
  '                 no-op; non-coord entries are preserved.)\n';

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
        ctx.stderr(INIT_HELP);
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
