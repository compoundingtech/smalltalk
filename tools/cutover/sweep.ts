#!/usr/bin/env -S node --experimental-strip-types --no-warnings
// tools/cutover/sweep.ts — walk a directory tree and apply the
// coord→st cutover rewriters to every `.mcp.json` and `pty.toml`
// underneath.
//
// Usage:
//   node --experimental-strip-types tools/cutover/sweep.ts <path> [<path> ...]
//                    [--dry-run] [--kind mcp-json|pty-toml|both]
//                    [--depth <n>]
//                    [--no-backup]
//
// Defaults:
//   --kind both              sweep .mcp.json AND pty.toml
//   --depth 4                walk 4 levels below each root
//   backup on                each rewritten file gets a
//                            `<name>.pre-cutover` copy alongside it
//
// The rewrite functions are pure — see `rewrite-mcp-json.ts` and
// `rewrite-pty-toml.ts`. This driver walks disk, reads, calls the
// rewriter, and writes back atomically (tmp+rename). Idempotent:
// running twice against the same tree is a no-op the second time.
//
// Exit codes:
//   0  clean sweep (files updated or already-migrated)
//   1  at least one file had a parse error and was skipped
//   2  bad CLI args

import {
  existsSync,
  readFileSync,
  readdirSync,
  renameSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { basename, join } from 'node:path';

import { rewriteMcpJson } from './rewrite-mcp-json.ts';
import { rewritePtyToml } from './rewrite-pty-toml.ts';

type Kind = 'mcp-json' | 'pty-toml' | 'both';

interface Args {
  roots: string[];
  kind: Kind;
  depth: number;
  dryRun: boolean;
  backup: boolean;
}

const HELP = `usage: sweep.ts <path> [<path> ...] [--dry-run] [--kind mcp-json|pty-toml|both] [--depth <n>] [--no-backup]

Walks each <path> and applies the coord→st cutover rewrites to every
.mcp.json and pty.toml underneath. --dry-run prints the actions that
would be taken but touches nothing on disk.
`;

function parseArgs(argv: readonly string[]): Args {
  const args: Args = {
    roots: [],
    kind: 'both',
    depth: 4,
    dryRun: false,
    backup: true,
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    switch (a) {
      case '-h':
      case '--help':
        process.stderr.write(HELP);
        process.exit(0);
      case '--dry-run':
        args.dryRun = true;
        break;
      case '--no-backup':
        args.backup = false;
        break;
      case '--depth': {
        const v = argv[++i];
        if (v === undefined || !/^[0-9]+$/.test(v)) {
          throw new Error('--depth must be a non-negative integer');
        }
        args.depth = Number(v);
        break;
      }
      case '--kind': {
        const v = argv[++i];
        if (v !== 'mcp-json' && v !== 'pty-toml' && v !== 'both') {
          throw new Error(
            `--kind must be 'mcp-json', 'pty-toml', or 'both' (got: ${v ?? '(missing)'})`
          );
        }
        args.kind = v;
        break;
      }
      default:
        if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
        args.roots.push(a);
    }
  }
  if (args.roots.length === 0) throw new Error('at least one <path> required');
  return args;
}

function walk(
  root: string,
  maxDepth: number,
  matches: (name: string) => boolean
): string[] {
  const found: string[] = [];
  const stack: { dir: string; depth: number }[] = [{ dir: root, depth: 0 }];
  while (stack.length > 0) {
    const { dir, depth } = stack.pop()!;
    let entries: string[];
    try {
      entries = readdirSync(dir);
    } catch {
      continue;
    }
    for (const entry of entries) {
      if (entry === 'node_modules' || entry === '.git') continue;
      const path = join(dir, entry);
      let st;
      try {
        st = statSync(path);
      } catch {
        continue;
      }
      if (st.isDirectory()) {
        if (depth < maxDepth) stack.push({ dir: path, depth: depth + 1 });
      } else if (st.isFile() && matches(basename(path))) {
        found.push(path);
      }
    }
  }
  return found.sort();
}

interface FileOutcome {
  path: string;
  status: 'changed' | 'unchanged' | 'skipped';
  actions: readonly string[];
  reason?: string;
}

function processFile(
  path: string,
  rewriter: (text: string) => { text: string; changed: boolean; actions: readonly string[] },
  args: Args
): FileOutcome {
  let raw: string;
  try {
    raw = readFileSync(path, 'utf8');
  } catch (err) {
    return {
      path,
      status: 'skipped',
      actions: [],
      reason: `read failed: ${(err as Error).message}`,
    };
  }
  let result;
  try {
    result = rewriter(raw);
  } catch (err) {
    return {
      path,
      status: 'skipped',
      actions: [],
      reason: `parse failed: ${(err as Error).message}`,
    };
  }
  if (!result.changed) {
    return { path, status: 'unchanged', actions: [] };
  }
  if (args.dryRun) {
    return { path, status: 'changed', actions: result.actions };
  }
  if (args.backup) {
    const backup = `${path}.pre-cutover`;
    // Skip-if-exists on the backup — a re-run shouldn't clobber the
    // pre-cutover snapshot from the first run.
    if (!existsSync(backup)) {
      try {
        writeFileSync(backup, raw);
      } catch (err) {
        return {
          path,
          status: 'skipped',
          actions: result.actions,
          reason: `backup write failed: ${(err as Error).message}`,
        };
      }
    }
  }
  // Atomic write via tmp+rename so a partial write can't leave the
  // file half-rewritten (an agent restart on such a file would fail
  // to parse the .mcp.json / .pty.toml).
  const tmp = `${path}.cutover.tmp`;
  try {
    writeFileSync(tmp, result.text);
    renameSync(tmp, path);
  } catch (err) {
    return {
      path,
      status: 'skipped',
      actions: result.actions,
      reason: `write failed: ${(err as Error).message}`,
    };
  }
  return { path, status: 'changed', actions: result.actions };
}

function formatOutcome(o: FileOutcome, args: Args): string {
  const badge =
    o.status === 'changed'
      ? args.dryRun
        ? '[dry-run would change]'
        : '[changed]'
      : o.status === 'skipped'
        ? '[skipped]'
        : '[unchanged]';
  const lines = [`${badge} ${o.path}`];
  if (o.reason !== undefined) lines.push(`    reason: ${o.reason}`);
  for (const a of o.actions) lines.push(`    · ${a}`);
  return lines.join('\n');
}

function main(): number {
  let args: Args;
  try {
    args = parseArgs(process.argv.slice(2));
  } catch (err) {
    process.stderr.write(`sweep.ts: ${(err as Error).message}\n\n${HELP}`);
    return 2;
  }

  const doMcp = args.kind === 'both' || args.kind === 'mcp-json';
  const doPty = args.kind === 'both' || args.kind === 'pty-toml';

  const outcomes: FileOutcome[] = [];
  for (const root of args.roots) {
    if (doMcp) {
      for (const path of walk(root, args.depth, (n) => n === '.mcp.json')) {
        outcomes.push(processFile(path, rewriteMcpJson, args));
      }
    }
    if (doPty) {
      for (const path of walk(root, args.depth, (n) => n === 'pty.toml')) {
        outcomes.push(processFile(path, rewritePtyToml, args));
      }
    }
  }

  let changed = 0;
  let unchanged = 0;
  let skipped = 0;
  for (const o of outcomes) {
    process.stdout.write(`${formatOutcome(o, args)}\n`);
    if (o.status === 'changed') changed += 1;
    else if (o.status === 'skipped') skipped += 1;
    else unchanged += 1;
  }
  const verb = args.dryRun ? 'would change' : 'changed';
  process.stdout.write(
    `\nsweep summary: ${changed} ${verb}, ${unchanged} already-migrated, ${skipped} skipped.\n`
  );
  return skipped > 0 ? 1 : 0;
}

process.exit(main());
