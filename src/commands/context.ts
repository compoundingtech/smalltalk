// commands/context.ts — `st context <verb>` for per-agent durable
// working-state (brief-024, context/ v1).
//
// Purpose. Solve the in-context-state loss leg of lossless-restart: a
// restart-from-summary / auto-compaction / crash used to wipe the
// model's memory of what it was mid-doing. `context/` persists that
// state on disk, outside the session jsonl, in the agent's smalltalk
// network folder (~/.local/state/st/<agent>/context/).
//
// Two surfaces, two shapes:
//   - now.md       — whole-file rewrite; `read now` prints it,
//                    `write` replaces it from stdin. Meant for
//                    "what I'm mid-doing" snapshots the model
//                    flushes at each meaningful state change.
//   - decisions/   — folder, one file per decision entry, named
//                    `<unix-ms>-<rand6>.md` — the same LAYOUT-004
//                    grammar as message inboxes. `append` creates a
//                    new file (no rewrite of any existing file), so:
//                      * two concurrent appends never race — each is
//                        an atomic create with a distinct rand6;
//                      * an interrupted append leaves at most a stale
//                        `.tmp` sibling, not a corrupted log;
//                      * `readdir` sorted by filename gives the log
//                        in chronological order for free.
//                    `read --decisions` concatenates all entries in
//                    that sorted order and re-emits them as a
//                    bulleted list (the file bodies are already
//                    bulleted lines, so it's a straight join).
//
// Absent-able (load-bearing for evals-claude's restart-continuity
// eval): every verb tolerates a missing `context/` folder. `read`
// returns empty text when the file/folder is absent. `append` and
// `write` lazy-create the folder. There is no `st context init`
// — you can go from zero to a first write without any ceremony, and
// the eval's control arm can just delete the folder to A/B against
// the treatment.
//
// Explicitly out of scope for v1 (v2 candidates surfaced by cos):
//   - No `now edit` verb — full-rewrite discipline prevents the
//     staleness that edit-in-place invites.
//   - No hook wiring here (PreCompact flush + SessionStart rehydrate
//     ship as a follow-up PR so we can iterate on the schema without
//     the hook plumbing in the way).
//   - No "standing jobs to re-establish on boot" schema — cos flagged
//     this as the way to close the dead-session-only-crons leg; v2.

import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  renameSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs';
import { randomBytes } from 'node:crypto';
import { dirname, join } from 'node:path';

import { invokedName, type CliContext } from '../cli-context.ts';
import {
  contextDecisionsDir,
  contextDir,
  contextNowPath,
  msNow,
  rand6,
  resolveIdentity,
} from '../common.ts';

// ─── Types ───────────────────────────────────────────────────────────────

export type ContextVerb = 'read' | 'write' | 'append';

export interface ContextReadInput {
  recipient?: string | undefined;
  /** Which surface to read. Default 'now'. */
  file?: 'now' | 'decisions' | 'full' | undefined;
  env: NodeJS.ProcessEnv;
  stRoot: string;
}

export interface ContextReadResult {
  identity: string;
  /** The requested surface. `full` returns both concatenated. */
  file: 'now' | 'decisions' | 'full';
  /** Contents; empty string when the surface is absent. */
  text: string;
  /**
   * True when the requested surface was absent (missing folder, missing
   * file, or `decisions/` with no `.md` entries). For `full`, true only
   * when BOTH surfaces were absent. Lets callers distinguish "empty
   * file" from "no file yet" without a second stat — the eval's
   * restart-continuity control arm consumes this flag directly.
   */
  absent: boolean;
}

export interface ContextWriteInput {
  recipient?: string | undefined;
  /** Whole-file rewrite content for now.md. */
  body: string;
  env: NodeJS.ProcessEnv;
  stRoot: string;
}

export interface ContextWriteResult {
  identity: string;
  path: string;
  /** Byte length of what was written. */
  bytes: number;
}

export interface ContextAppendInput {
  recipient?: string | undefined;
  /** The decision text — one line, no leading `- `; we add the bullet. */
  decision: string;
  /** The "why" — kept separate so callers must think about the reason. */
  why: string;
  /**
   * ISO timestamp to stamp the entry body with. Callers supply this so
   * the core stays deterministic under test (no clock reach from the
   * body). CLI wrapper defaults to `new Date().toISOString()`.
   */
  timestamp: string;
  /**
   * Test seam. When provided, override the generated `<unix-ms>-<rand6>.md`
   * filename so tests can assert exact bytes on disk. Real callers
   * (CLI + MCP + SDK handle) never set this — we derive it from
   * `timestamp` + a fresh rand6.
   */
  filename?: string | undefined;
  env: NodeJS.ProcessEnv;
  stRoot: string;
}

export interface ContextAppendResult {
  identity: string;
  /** Absolute path of the entry file that was written. */
  path: string;
  /** Basename of {@link path} — `<unix-ms>-<rand6>.md`. */
  filename: string;
  /**
   * The bulleted line that was written to the file (also what
   * `read --decisions` will emit for this entry). No trailing `\n`.
   */
  line: string;
}

// ─── Core ─────────────────────────────────────────────────────────────────

/**
 * Read one of the context surfaces. Absent-able: a missing folder /
 * file / empty `decisions/` returns `text: ''` + `absent: true` so
 * callers can distinguish "restart with no prior context" from
 * "restart with empty context."
 */
export function cmdContextRead(input: ContextReadInput): ContextReadResult {
  const identity = resolveIdentity({
    explicit: input.recipient,
    env: input.env,
    stRoot: input.stRoot,
  });
  const which = input.file ?? 'now';

  if (which === 'now') {
    const { text, absent } = readIfPresent(
      contextNowPath(identity, input.stRoot)
    );
    return { identity, file: 'now', text, absent };
  }
  if (which === 'decisions') {
    const { text, absent } = readDecisionsFolder(identity, input.stRoot);
    return { identity, file: 'decisions', text, absent };
  }
  // `full`: now.md then decisions/*.md, separated by headings so the
  // reader can tell what came from where. Absent-flag is true iff
  // BOTH surfaces were missing — a partial rehydrate is still "present".
  const now = readIfPresent(contextNowPath(identity, input.stRoot));
  const dec = readDecisionsFolder(identity, input.stRoot);
  const parts: string[] = [];
  if (!now.absent) {
    parts.push('# now.md', now.text);
  }
  if (!dec.absent) {
    if (parts.length > 0) parts.push('');
    parts.push('# decisions/', dec.text);
  }
  return {
    identity,
    file: 'full',
    text: parts.join('\n'),
    absent: now.absent && dec.absent,
  };
}

/**
 * Whole-file rewrite of `now.md`. Atomic via tmp + rename so a
 * concurrent reader can't see a partial file — matters because the
 * SessionStart hook reads this on every boot and we don't want a
 * mid-write moment to inject a truncated `<context>` block.
 */
export function cmdContextWrite(
  input: ContextWriteInput
): ContextWriteResult {
  const identity = resolveIdentity({
    explicit: input.recipient,
    env: input.env,
    stRoot: input.stRoot,
  });
  const path = contextNowPath(identity, input.stRoot);
  ensureContextDir(identity, input.stRoot);
  // Normalize trailing newline so the file always ends with \n. Keeps
  // downstream tools happy (git diff, `cat`), matches the shape the
  // model writes when the body already ends with a newline.
  const body = input.body.endsWith('\n') ? input.body : input.body + '\n';
  writeAtomic(path, body);
  return { identity, path, bytes: body.length };
}

/**
 * Append one decision + why entry to `decisions/`. Semantics:
 *   - Each entry lives in its own file named `<unix-ms>-<rand6>.md`
 *     (LAYOUT-004 grammar). The unix-ms is derived from the caller's
 *     `timestamp` so filename-sort order matches ISO-time order in the
 *     bodies; a rand6 suffix prevents same-ms collisions.
 *   - The body is one bulleted line — the same shape a hand-rolled
 *     `>> decisions.md` append would produce in the old single-file
 *     v1 draft. Concatenating the files in filename-sort order gives
 *     back a bulleted list; `read --decisions` does exactly that.
 *   - Rejects `\n` in either field: a decision that spans lines
 *     belongs in a note or a doc, not in this log. Also rejects empty
 *     strings on either.
 */
export function cmdContextAppend(
  input: ContextAppendInput
): ContextAppendResult {
  const identity = resolveIdentity({
    explicit: input.recipient,
    env: input.env,
    stRoot: input.stRoot,
  });
  const decision = input.decision.trim();
  const why = input.why.trim();
  if (decision.length === 0) {
    throw new Error('--decision is required and cannot be empty');
  }
  if (why.length === 0) {
    throw new Error('--why is required and cannot be empty');
  }
  if (decision.includes('\n') || why.includes('\n')) {
    throw new Error(
      "context append: --decision and --why must be single lines. Multi-line reasoning belongs in a doc; the log is a scannable list."
    );
  }
  const line = `- ${input.timestamp} ${trimTrailingPeriod(decision)}. why: ${trimTrailingPeriod(why)}.`;

  // Derive filename-ms from the body timestamp so filenames sort in
  // strict ISO-time order (a burst of appends within the same second
  // still sorts correctly because the rand6 suffix disambiguates).
  // If timestamp isn't parseable, fall back to msNow() — better a
  // "now" filename than a NaN-prefixed one that breaks sort.
  const parsedMs = Date.parse(input.timestamp);
  const ms = Number.isFinite(parsedMs) ? parsedMs : msNow();
  const filename = input.filename ?? `${ms}-${rand6()}.md`;

  const dir = contextDecisionsDir(identity, input.stRoot);
  mkdirSync(dir, { recursive: true });
  const path = join(dir, filename);
  writeAtomic(path, line + '\n');
  return { identity, path, filename, line };
}

// ─── Helpers ─────────────────────────────────────────────────────────────

function readIfPresent(path: string): { text: string; absent: boolean } {
  if (!existsSync(path)) return { text: '', absent: true };
  try {
    return { text: readFileSync(path, 'utf8'), absent: false };
  } catch {
    // Present-but-unreadable is different from absent — treat as absent
    // so callers don't rehydrate garbage, but also don't surface a
    // hard error. The eval's control arm gets the same shape either way.
    return { text: '', absent: true };
  }
}

/**
 * Enumerate + concatenate the per-decision `.md` files in
 * `<context>/decisions/`. Missing folder OR zero entries → `absent`.
 * Sort is plain lexicographic on filename; since names are
 * `<unix-ms>-<rand6>.md` and ms values are fixed-width for any date
 * in this century, lex-sort equals chronological sort.
 *
 * Each file's body is a bulleted line already (`- <ISO> …`), so the
 * concatenation is a straight join with newlines — no re-bulleting.
 * Trailing newlines on individual files are normalized so the final
 * text ends with exactly one `\n`.
 */
function readDecisionsFolder(
  identity: string,
  root: string
): { text: string; absent: boolean } {
  const dir = contextDecisionsDir(identity, root);
  if (!existsSync(dir)) return { text: '', absent: true };
  let entries: string[];
  try {
    entries = readdirSync(dir)
      .filter((n) => n.endsWith('.md'))
      .sort();
  } catch {
    // Present-but-unreadable directory — treat as absent, same rationale
    // as readIfPresent. The eval's control arm needs a stable "no
    // context" surface regardless of transient filesystem errors.
    return { text: '', absent: true };
  }
  if (entries.length === 0) return { text: '', absent: true };

  const lines: string[] = [];
  for (const name of entries) {
    let raw: string;
    try {
      raw = readFileSync(join(dir, name), 'utf8');
    } catch {
      // Skip individual unreadable files. A single corrupt entry must
      // not black-hole the rest of the log.
      continue;
    }
    // Each file is meant to be one line + trailing newline. Trim
    // trailing whitespace so the join produces a canonical list.
    const line = raw.replace(/\s+$/, '');
    if (line.length > 0) lines.push(line);
  }
  if (lines.length === 0) return { text: '', absent: true };
  return { text: lines.join('\n') + '\n', absent: false };
}

function ensureContextDir(identity: string, root: string): void {
  mkdirSync(contextDir(identity, root), { recursive: true });
}

function writeAtomic(path: string, body: string): void {
  const dir = dirname(path);
  const tmp = join(
    dir,
    `.context.tmp-${process.pid}-${randomBytes(3).toString('hex')}`
  );
  try {
    writeFileSync(tmp, body);
    renameSync(tmp, path);
  } catch (err) {
    try {
      unlinkSync(tmp);
    } catch {
      // ignore
    }
    throw err;
  }
}

function trimTrailingPeriod(s: string): string {
  return s.endsWith('.') ? s.slice(0, -1) : s;
}

// ─── CLI wrapper ─────────────────────────────────────────────────────────

function contextUsage(name: string): string {
  return (
    `usage: ${name} context <verb> [args...]\n\n` +
    '  read [<identity>] [--decisions | --full]\n' +
    '                           print now.md (default), decisions/ log, or both.\n' +
    '                           Absent files print nothing (exit 0) so the\n' +
    '                           SessionStart hook can `cat` unconditionally.\n' +
    '  write [<identity>]       whole-file rewrite of now.md from stdin.\n' +
    '                           Creates the context/ folder if absent.\n' +
    '  append [<identity>] --decision "<text>" --why "<text>"\n' +
    '                           append one entry to decisions/ as a new file\n' +
    '                           named <unix-ms>-<rand6>.md. No file rewrites.\n\n' +
    '  Layout: $ST_ROOT/<identity>/context/\n' +
    '           ├── now.md            whole-file, last-write-wins snapshot\n' +
    '           └── decisions/        one file per entry\n' +
    '               └── <unix-ms>-<rand6>.md\n' +
    '  brief-024 (context/ v1): the in-context-state leg of lossless-restart.\n'
  );
}

export async function cmdContextCli(
  args: readonly string[],
  ctx: CliContext
): Promise<number> {
  const sub = args[0];
  if (sub === undefined || sub === '-h' || sub === '--help') {
    ctx.stderr(contextUsage(invokedName(ctx.env)));
    return sub === undefined ? 2 : 0;
  }
  const rest = args.slice(1);
  switch (sub) {
    case 'read':
      return cliRead(rest, ctx);
    case 'write':
      return await cliWrite(rest, ctx);
    case 'append':
      return cliAppend(rest, ctx);
    default:
      ctx.stderr(`st context: unknown subcommand: ${sub}\n\n`);
      ctx.stderr(contextUsage(invokedName(ctx.env)));
      return 2;
  }
}

function cliRead(args: readonly string[], ctx: CliContext): number {
  let recipient: string | undefined;
  let file: 'now' | 'decisions' | 'full' = 'now';
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '--decisions':
        file = 'decisions';
        break;
      case '--full':
        file = 'full';
        break;
      default:
        if (a.startsWith('-')) {
          throw new Error(`unknown flag: ${a}`);
        }
        if (recipient === undefined) recipient = a;
        else throw new Error(`unexpected arg: ${a}`);
    }
  }
  const r = cmdContextRead({
    ...(recipient !== undefined && { recipient }),
    file,
    env: ctx.env,
    stRoot: ctx.stRoot,
  });
  // Print the text as-is (including empty). Absent files exit 0 with
  // empty output — the SessionStart hook can `cat $(st context
  // read)` unconditionally without a special-case for first-boot
  // agents.
  ctx.stdout(r.text);
  return 0;
}

async function cliWrite(
  args: readonly string[],
  ctx: CliContext
): Promise<number> {
  let recipient: string | undefined;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
    if (recipient === undefined) recipient = a;
    else throw new Error(`unexpected arg: ${a}`);
  }
  const buf = await ctx.readStdin();
  const body = buf.toString('utf8');
  const r = cmdContextWrite({
    ...(recipient !== undefined && { recipient }),
    body,
    env: ctx.env,
    stRoot: ctx.stRoot,
  });
  ctx.stdout(`wrote ${r.bytes} bytes to ${r.path}\n`);
  return 0;
}

function cliAppend(args: readonly string[], ctx: CliContext): number {
  let recipient: string | undefined;
  let decision: string | undefined;
  let why: string | undefined;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '--decision':
        if (i + 1 >= args.length) {
          throw new Error('--decision requires a value');
        }
        decision = args[++i];
        break;
      case '--why':
        if (i + 1 >= args.length) {
          throw new Error('--why requires a value');
        }
        why = args[++i];
        break;
      default:
        if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
        if (recipient === undefined) recipient = a;
        else throw new Error(`unexpected arg: ${a}`);
    }
  }
  if (decision === undefined) {
    throw new Error('--decision <text> is required');
  }
  if (why === undefined) {
    throw new Error('--why <text> is required');
  }
  const r = cmdContextAppend({
    ...(recipient !== undefined && { recipient }),
    decision,
    why,
    timestamp: new Date().toISOString(),
    env: ctx.env,
    stRoot: ctx.stRoot,
  });
  ctx.stdout(`${r.line}\n`);
  return 0;
}
