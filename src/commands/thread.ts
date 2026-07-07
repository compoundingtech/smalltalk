// commands/thread.ts — walk the reply chain for a message.
//
// The walk is global: a two-party conversation lives across both
// identity trees (alice's outbound to bob is under <bob>/inbox|archive,
// bob's reply to alice is under <alice>/inbox|archive). We scan every
// $COORD_ROOT/*/{inbox,archive}/ directory.

import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';

import {
  archiveDir,
  inboxDir,
  parseFrontmatter,
  resolveIdentity,
  validFilename,
} from '../common.ts';
import {
  InvalidFilenameError,
  MessageNotFoundError,
} from '../errors.ts';

export interface ThreadInput {
  recipient?: string | undefined;
  filename: string;
  /** When true, indented hierarchical output. Otherwise flat chronological. */
  tree?: boolean;
  env: NodeJS.ProcessEnv;
  stRoot: string;
}

export interface ThreadLine {
  filename: string;
  from: string;
  subject: string;
  /** Indent depth (0 in flat mode; tree depth in --tree mode). */
  depth: number;
}

export interface ThreadResult {
  lines: ThreadLine[];
}

export function cmdThread(input: ThreadInput): ThreadResult {
  if (!input.filename) throw new Error('<filename> required');

  // resolveIdentity is the validation hint for the seed location even
  // though the walk itself is global. Lenient on explicit <other>:
  // a peer's folder on this machine may be partial.
  resolveIdentity({
    explicit: input.recipient,
    env: input.env,
    stRoot: input.stRoot,
    ...(input.recipient ? { policy: 'lenient' as const } : {}),
  });

  if (!validFilename(input.filename)) {
    throw new InvalidFilenameError(input.filename);
  }

  const seedPath = locateAnywhere(input.stRoot, input.filename);
  if (seedPath === undefined) {
    throw new MessageNotFoundError('', input.filename);
  }

  // Walk ancestors: seed → parent → grandparent → … (cycle-guarded).
  const ancestorsTopDown: string[] = [];
  const inChain = new Set<string>();
  let current: string | undefined = input.filename;
  while (current !== undefined) {
    const path = locateAnywhere(input.stRoot, current);
    if (path === undefined) break;
    ancestorsTopDown.unshift(current);
    inChain.add(current);
    const parentRaw = readFm(path).fm['in-reply-to'];
    if (typeof parentRaw !== 'string' || parentRaw.length === 0) break;
    if (!validFilename(parentRaw)) break;
    if (inChain.has(parentRaw)) break; // cycle guard
    current = parentRaw;
  }

  if (input.tree === true) {
    return treeOutput(input.stRoot, input.filename, ancestorsTopDown);
  }
  return flatOutput(input.stRoot, input.filename, ancestorsTopDown);
}

// ─── Output modes ───────────────────────────────────────────────────────

function flatOutput(
  stRoot: string,
  seed: string,
  ancestors: readonly string[]
): ThreadResult {
  const reachable = new Set<string>(ancestors);
  collectDescendants(stRoot, seed, reachable);

  const sorted = [...reachable].sort();
  const lines: ThreadLine[] = sorted.map((name) => ({
    filename: name,
    ...readFromSubject(stRoot, name),
    depth: 0,
  }));
  return { lines };
}

function treeOutput(
  stRoot: string,
  seed: string,
  ancestors: readonly string[]
): ThreadResult {
  const lines: ThreadLine[] = [];
  const printed = new Set<string>();
  // Ancestors with deepening indent.
  for (let i = 0; i < ancestors.length; i++) {
    const name = ancestors[i]!;
    lines.push({
      filename: name,
      ...readFromSubject(stRoot, name),
      depth: i,
    });
    printed.add(name);
  }
  descendForTree(stRoot, seed, ancestors.length, printed, lines);
  return { lines };
}

// ─── Walk helpers ───────────────────────────────────────────────────────

function locateAnywhere(stRoot: string, name: string): string | undefined {
  let topEntries: string[];
  try {
    topEntries = readdirSync(stRoot);
  } catch {
    return undefined;
  }
  for (const id of topEntries) {
    for (const sub of ['inbox', 'archive']) {
      const candidate = join(stRoot, id, sub, name);
      try {
        if (statSync(candidate).isFile()) return candidate;
      } catch {
        // skip missing
      }
    }
  }
  return undefined;
}

function readFm(path: string): { fm: Record<string, unknown>; body: string } {
  try {
    return parseFrontmatter(readFileSync(path, 'utf8'));
  } catch {
    return { fm: {}, body: '' };
  }
}

function readFromSubject(
  stRoot: string,
  name: string
): { from: string; subject: string } {
  const path = locateAnywhere(stRoot, name);
  if (path === undefined) return { from: '', subject: '' };
  const fm = readFm(path).fm;
  const from = typeof fm.from === 'string' ? fm.from : '';
  const subject = typeof fm.subject === 'string' ? fm.subject : '';
  return { from, subject };
}

function findChildrenOf(stRoot: string, parent: string): string[] {
  const children = new Set<string>();
  let topEntries: string[];
  try {
    topEntries = readdirSync(stRoot);
  } catch {
    return [];
  }
  for (const id of topEntries) {
    for (const sub of ['inbox', 'archive']) {
      const dir = join(stRoot, id, sub);
      let entries: string[];
      try {
        entries = readdirSync(dir);
      } catch {
        continue;
      }
      for (const name of entries) {
        if (!validFilename(name)) continue;
        if (children.has(name)) continue;
        const fm = readFm(join(dir, name)).fm;
        if (fm['in-reply-to'] === parent) children.add(name);
      }
    }
  }
  return [...children].sort();
}

function collectDescendants(
  stRoot: string,
  parent: string,
  set: Set<string>
): void {
  // Track which children we actually added; only recurse into those, so
  // a cycle in in-reply-to doesn't bounce between the same two names.
  const fresh: string[] = [];
  for (const name of findChildrenOf(stRoot, parent)) {
    if (set.has(name)) continue;
    set.add(name);
    fresh.push(name);
  }
  for (const name of fresh) {
    collectDescendants(stRoot, name, set);
  }
}

function descendForTree(
  stRoot: string,
  parent: string,
  depth: number,
  printed: Set<string>,
  out: ThreadLine[]
): void {
  const children = findChildrenOf(stRoot, parent).filter(
    (c) => !printed.has(c)
  );
  for (const name of children) {
    out.push({ filename: name, ...readFromSubject(stRoot, name), depth });
    printed.add(name);
    descendForTree(stRoot, name, depth + 1, printed, out);
  }
}

// ─── Positional disambiguation ──────────────────────────────────────────

/**
 * Single-positional disambiguation by `.md` suffix (identity names
 * can't contain `.` per LAYOUT-004). Pre-brief-017a this used the
 * strict `validFilename` grammar, which sent a typo'd filename down
 * the identity path and produced the misleading "<filename>
 * required" message.
 */
export function splitThreadPositionals(
  positional: readonly string[]
): { recipient?: string | undefined; filename?: string | undefined } {
  switch (positional.length) {
    case 0:
      return {};
    case 1: {
      const v = positional[0]!;
      if (v.endsWith('.md')) return { filename: v };
      return { recipient: v };
    }
    case 2:
      return { recipient: positional[0], filename: positional[1] };
    default:
      throw new Error('too many arguments');
  }
}

// ─── Format a single line for stdout ────────────────────────────────────

export function formatThreadLine(line: ThreadLine): string {
  const indent = '  '.repeat(line.depth);
  return `${indent}${line.filename}\t${line.from}\t${line.subject}`;
}

export { cmdThread as cmdThreadCore };

// ─── CLI wrapper ────────────────────────────────────────────────────────

import { invokedName, type CliContext } from '../cli-context.ts';

export function cmdThreadCli(args: readonly string[], ctx: CliContext): number {
  let tree = false;
  const positional: string[] = [];
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '--tree':
        tree = true;
        break;
      case '-h':
      case '--help':
        ctx.stderr(
          `usage: ${invokedName(ctx.env)} message thread [<identity>] <filename> [--tree]\n`
        );
        return 0;
      default:
        if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
        positional.push(a);
    }
  }
  const { recipient, filename } = splitThreadPositionals(positional);
  if (filename === undefined) throw new Error('<filename> required');
  const r = cmdThread({
    ...(recipient !== undefined && { recipient }),
    filename,
    tree,
    env: ctx.env,
    stRoot: ctx.stRoot,
  });
  for (const line of r.lines) ctx.stdout(`${formatThreadLine(line)}\n`);
  return 0;
}
