// commands/task.ts — manage a single identity's own task list.
//
// Per LAYOUT-015: `tasks/` is the third optional folder under each
// identity. Mutable, single-writer (only the owning identity writes;
// peers read via sync). The owner edits files freely as work
// progresses; status transitions (`todo → doing → done | blocked`)
// publish agent state to the rest of the coord world.
//
// Subcommands (each pairs a typed core function with a CLI wrapper):
//   coord task new "<title>" [--priority P] [--tag T,T] [--due Y-M-D]
//                            [--from-message <inbox-filename>] [--no-edit]
//   coord task ls [--status STATE] [--tag T] [--priority P] [--json]
//   coord task status <filename> <STATE>
//   coord task done <filename>
//   coord task edit <filename>

import { spawnSync } from 'node:child_process';
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  renameSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { randomBytes } from 'node:crypto';
import { join } from 'node:path';

import type { CliContext } from '../cli-context.ts';
import {
  archiveDir,
  emitFrontmatter,
  inboxDir,
  msNow,
  parseFrontmatter,
  resolveIdentity,
  tasksDir,
} from '../common.ts';
import {
  InvalidTaskStateError,
  InvalidTaskTitleError,
  TaskNotFoundError,
  TasksSingleWriterError,
  type TaskState,
} from '../errors.ts';

export const TASK_STATES: readonly TaskState[] = [
  'todo',
  'doing',
  'done',
  'blocked',
];
const PRIORITIES = new Set(['low', 'normal', 'high']);

// ─── Shape ──────────────────────────────────────────────────────────────

export interface TaskRecord {
  filename: string;
  title: string;
  status: TaskState | string;
  priority: string | null;
  tags: string[];
  due: string | null;
  body: string;
}

// ─── Helpers ────────────────────────────────────────────────────────────

/** lowercase, hyphenate, collapse, trim, truncate to 32. */
function slugify(title: string): string {
  return title
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 32);
}

/**
 * Parse a tasks file as a TaskRecord. Permissive: missing
 * frontmatter is treated as untyped (body only); unknown fields are
 * preserved on disk but ignored by the structured shape.
 */
function readTaskFile(path: string, filename: string): TaskRecord {
  const text = readFileSync(path, 'utf8');
  const { fm, body } = parseFrontmatter(text);
  const status =
    typeof fm.status === 'string' && fm.status.length > 0
      ? fm.status
      : 'todo';
  const priority =
    typeof fm.priority === 'string' && fm.priority.length > 0
      ? fm.priority
      : null;
  let tags: string[] = [];
  if (Array.isArray(fm.tags)) {
    tags = fm.tags.map((t) => String(t));
  } else if (typeof fm.tags === 'string' && fm.tags.length > 0) {
    tags = parseTagsScalar(fm.tags);
  }
  const due =
    typeof fm.due === 'string' && fm.due.length > 0 ? fm.due : null;
  const title = extractTitle(body) ?? filenameToTitle(filename);
  return { filename, title, status, priority, tags, due, body };
}

/** Pull the first `# heading` line out of the body, if any. */
function extractTitle(body: string): string | null {
  for (const line of body.split('\n')) {
    const m = /^#\s+(.+?)\s*$/.exec(line);
    if (m && m[1] !== undefined) return m[1];
    // Stop at the first non-blank non-heading line.
    if (line.trim().length > 0) return null;
  }
  return null;
}

/** Filename fallback for the title: strip the `<unix-ms>-` prefix
 * and `.md` suffix, hyphens → spaces. */
function filenameToTitle(filename: string): string {
  const base = filename.replace(/\.md$/i, '');
  const stripped = base.replace(/^[0-9]{10,}-/, '');
  return stripped.replace(/-+/g, ' ').trim() || base;
}

function parseTagsScalar(s: string): string[] {
  const trimmed = s.replace(/^\[/, '').replace(/\]$/, '');
  return trimmed
    .split(',')
    .map((t) => t.trim().replace(/^"(.*)"$/, '$1').replace(/^'(.*)'$/, '$1'))
    .filter((t) => t.length > 0);
}

/** Write a tasks file atomically via tmp + rename. Unlike inbox writes
 * we expect to overwrite an existing file (mutations). */
function atomicWriteTask(path: string, content: string | Buffer): void {
  const tmp = `${path}.tmp-${process.pid}-${randomBytes(3).toString('hex')}`;
  writeFileSync(tmp, content);
  renameSync(tmp, path);
}

/** Ensure `<identity>/tasks/` exists. Lazily creates the README.md
 * stub on first write (only when actually creating the dir). */
function ensureTasksDir(identity: string, root: string): string {
  const dir = tasksDir(identity, root);
  const isNew = !existsSync(dir);
  mkdirSync(dir, { recursive: true });
  const readme = join(dir, 'README.md');
  if (isNew && !existsSync(readme)) {
    const body =
      `# ${identity}'s tasks\n\n` +
      `Created by coord. Use \`coord task ls\` to list, ` +
      `\`coord task new "<title>"\` to add.\n`;
    writeFileSync(readme, body);
  }
  return dir;
}

/**
 * Locate `<filename>` under any identity's `tasks/` folder. Used by
 * the single-writer error path to give a useful "this task belongs
 * to <other>" message instead of "not found."
 */
function findTaskOwner(root: string, filename: string): string | undefined {
  let entries: string[];
  try {
    entries = readdirSync(root);
  } catch {
    return undefined;
  }
  for (const id of entries) {
    const path = join(root, id, 'tasks', filename);
    if (existsSync(path)) return id;
  }
  return undefined;
}

// ─── coord task new ─────────────────────────────────────────────────────

export interface TaskNewInput {
  title: string;
  priority?: string | undefined;
  tags?: string | string[] | undefined;
  due?: string | undefined;
  /** Inbox/archive filename whose body seeds the task body. */
  fromMessage?: string | undefined;
  /** When false (default), opens $EDITOR via {@link editorSpawn} after
   * writing the file. When true, the writer returns immediately. */
  noEdit?: boolean;
  /** Test seam: invoked instead of spawnSync(editor, [path]). */
  editorSpawn?: (editor: string, path: string) => void;

  env: NodeJS.ProcessEnv;
  coordRoot: string;
}

export interface TaskNewResult {
  filename: string;
  path: string;
  identity: string;
}

export function cmdTaskNew(input: TaskNewInput): TaskNewResult {
  const slug = slugify(input.title);
  if (slug.length === 0) {
    throw new InvalidTaskTitleError(input.title);
  }

  const identity = resolveIdentity({
    env: input.env,
    coordRoot: input.coordRoot,
  });

  if (input.priority !== undefined && input.priority !== '') {
    if (!PRIORITIES.has(input.priority)) {
      // Reuse the existing priority guard from send.
      const err = new Error(`invalid priority: ${input.priority}`);
      (err as { code?: string }).code = 'INVALID_PRIORITY';
      throw err;
    }
  }

  const dir = ensureTasksDir(identity, input.coordRoot);
  const filename = `${msNow()}-${slug}.md`;
  const path = join(dir, filename);

  // Frontmatter: always include `status: todo`. Priority/tags/due are
  // emitted only when set so the file stays minimal.
  const fm: Record<string, unknown> = { status: 'todo' };
  if (input.priority !== undefined && input.priority !== '') {
    fm.priority = input.priority;
  }
  const tags = normalizeTags(input.tags);
  if (tags.length > 0) fm.tags = tags;
  if (input.due !== undefined && input.due !== '') {
    fm.due = input.due;
  }

  // Body: # title + (optional seed body from a coord message).
  let body = `# ${input.title}\n\n`;
  if (input.fromMessage !== undefined && input.fromMessage !== '') {
    const seed = readMessageBody(
      identity,
      input.fromMessage,
      input.coordRoot
    );
    if (seed.length > 0) {
      body += seed.endsWith('\n') ? seed : `${seed}\n`;
    }
  }

  const content = emitFrontmatter(fm, body);
  atomicWriteTask(path, content);

  // Editor side-effect (skipped in tests or with --no-edit).
  if (input.noEdit !== true) {
    const editor = input.env.EDITOR ?? input.env.VISUAL;
    if (editor !== undefined && editor.length > 0) {
      if (input.editorSpawn !== undefined) {
        input.editorSpawn(editor, path);
      } else {
        spawnSync(editor, [path], { stdio: 'inherit' });
      }
    }
  }

  return { filename, path, identity };
}

function normalizeTags(input: string | string[] | undefined): string[] {
  if (input === undefined) return [];
  // Comma-split on every element so all three forms collapse to the
  // same flat tag list:
  //   --tag a,b,c          → ['a,b,c']      (one element with commas)
  //   --tag a --tag b      → ['a', 'b']     (two elements)
  //   --tag "a,b" --tag c  → ['a,b', 'c']   (mixed)
  // Pre-brief-017a only the single-string form was split, so
  // `--tag a,b` from the CLI ended up as one tag "a,b".
  const raw = Array.isArray(input) ? input.flatMap((s) => s.split(',')) : input.split(',');
  return raw.map((t) => t.trim()).filter((t) => t.length > 0);
}

function readMessageBody(
  identity: string,
  filename: string,
  root: string
): string {
  for (const dir of [
    inboxDir(identity, root),
    archiveDir(identity, root),
  ]) {
    const path = join(dir, filename);
    if (existsSync(path)) {
      const text = readFileSync(path, 'utf8');
      return parseFrontmatter(text).body;
    }
  }
  // Quiet fall-through: --from-message with no matching file is a
  // no-op rather than a hard error. Mirrors coord_read's permissive
  // semantics; the task body just won't carry the seed.
  return '';
}

// ─── coord task ls ──────────────────────────────────────────────────────

export interface TaskLsInput {
  /** Filter to tasks whose `status:` equals this. */
  status?: string | undefined;
  /** Filter to tasks whose tags include this. */
  tag?: string | undefined;
  /** Filter to tasks whose `priority:` equals this. */
  priority?: string | undefined;

  env: NodeJS.ProcessEnv;
  coordRoot: string;
}

export interface TaskLsResult {
  identity: string;
  items: TaskRecord[];
}

export function cmdTaskLs(input: TaskLsInput): TaskLsResult {
  const identity = resolveIdentity({
    env: input.env,
    coordRoot: input.coordRoot,
  });
  const items = listTaskRecords(identity, input.coordRoot, {
    ...(input.status !== undefined && { status: input.status }),
    ...(input.tag !== undefined && { tag: input.tag }),
    ...(input.priority !== undefined && { priority: input.priority }),
  });
  return { identity, items };
}

/**
 * Walk `<id>/tasks/` and return matching TaskRecords. Plain file
 * I/O, no resolveIdentity — safe for the cross-tree `coord tasks`
 * read path (which shouldn't auto-create folders for identities
 * it's merely observing).
 *
 * Missing `tasks/` folder returns `[]`. Missing `<id>` folder
 * returns `[]`. Caller is responsible for any identity-name
 * validation; this helper just walks bytes.
 */
export function listTaskRecords(
  identity: string,
  root: string,
  filters: { status?: string; tag?: string; priority?: string } = {}
): TaskRecord[] {
  const dir = tasksDir(identity, root);
  if (!existsSync(dir)) return [];
  let names: string[];
  try {
    names = readdirSync(dir);
  } catch {
    // Permission-denied (chmod 000), I/O error, etc — degrade
    // gracefully. The brief-016 read-side resilience contract is
    // "missing/unreadable folders yield no rows; never crash."
    return [];
  }
  const items: TaskRecord[] = [];
  for (const name of names.sort()) {
    if (name === 'README.md') continue;
    if (!name.endsWith('.md')) continue;
    let rec: TaskRecord;
    try {
      rec = readTaskFile(join(dir, name), name);
    } catch {
      continue;
    }
    if (
      filters.status !== undefined &&
      filters.status !== '' &&
      rec.status !== filters.status
    ) {
      continue;
    }
    if (
      filters.tag !== undefined &&
      filters.tag !== '' &&
      !rec.tags.includes(filters.tag)
    ) {
      continue;
    }
    if (
      filters.priority !== undefined &&
      filters.priority !== '' &&
      rec.priority !== filters.priority
    ) {
      continue;
    }
    items.push(rec);
  }
  return items;
}

// ─── coord task status / done ───────────────────────────────────────────

export interface TaskStatusInput {
  filename: string;
  state: string;
  env: NodeJS.ProcessEnv;
  coordRoot: string;
}

export interface TaskStatusResult {
  identity: string;
  filename: string;
  previousStatus: string;
  newStatus: TaskState;
}

export function cmdTaskStatus(input: TaskStatusInput): TaskStatusResult {
  if (!TASK_STATES.includes(input.state as TaskState)) {
    throw new InvalidTaskStateError(input.state);
  }
  const identity = resolveIdentity({
    env: input.env,
    coordRoot: input.coordRoot,
  });
  const path = join(tasksDir(identity, input.coordRoot), input.filename);
  if (!existsSync(path)) {
    const other = findTaskOwner(input.coordRoot, input.filename);
    if (other !== undefined && other !== identity) {
      throw new TasksSingleWriterError(identity, input.filename, other);
    }
    throw new TaskNotFoundError(identity, input.filename);
  }

  const text = readFileSync(path, 'utf8');
  const { fm, body } = parseFrontmatter(text);
  const previousStatus =
    typeof fm.status === 'string' && fm.status.length > 0
      ? fm.status
      : 'todo';
  fm.status = input.state;
  atomicWriteTask(path, emitFrontmatter(fm, body));
  return {
    identity,
    filename: input.filename,
    previousStatus,
    newStatus: input.state as TaskState,
  };
}

// ─── coord task edit ────────────────────────────────────────────────────

export interface TaskEditInput {
  filename: string;
  editorSpawn?: (editor: string, path: string) => void;
  env: NodeJS.ProcessEnv;
  coordRoot: string;
}

export interface TaskEditResult {
  identity: string;
  filename: string;
  path: string;
}

export function cmdTaskEdit(input: TaskEditInput): TaskEditResult {
  const identity = resolveIdentity({
    env: input.env,
    coordRoot: input.coordRoot,
  });
  const path = join(tasksDir(identity, input.coordRoot), input.filename);
  if (!existsSync(path)) {
    const other = findTaskOwner(input.coordRoot, input.filename);
    if (other !== undefined && other !== identity) {
      throw new TasksSingleWriterError(identity, input.filename, other);
    }
    throw new TaskNotFoundError(identity, input.filename);
  }
  const editor = input.env.EDITOR ?? input.env.VISUAL;
  if (editor !== undefined && editor.length > 0) {
    if (input.editorSpawn !== undefined) {
      input.editorSpawn(editor, path);
    } else {
      spawnSync(editor, [path], { stdio: 'inherit' });
    }
  }
  return { identity, filename: input.filename, path };
}

// Touch statSync to keep the import used (read-side timing for
// `tasks --watch` lives in tasks.ts; we don't use it here directly).
void statSync;

// ─── CLI wrapper ────────────────────────────────────────────────────────

const TASK_HELP =
  'usage: coord task <subcommand> [args...]\n\n' +
  '  new "<title>" [--priority P] [--tag T,T] [--due YYYY-MM-DD]\n' +
  '                [--from-message <inbox-filename>] [--no-edit]\n' +
  '  ls [--status STATE] [--tag T] [--priority P] [--json]\n' +
  '  status <filename> <STATE>\n' +
  '  done <filename>\n' +
  '  edit <filename>\n\n' +
  '  Operates on $COORD_IDENTITY/tasks/. tasks/ is single-writer:\n' +
  '  only the identity owner edits; peers read via sync.\n';

export async function cmdTaskCli(
  args: readonly string[],
  ctx: CliContext
): Promise<number> {
  const sub = args[0];
  if (sub === undefined || sub === 'help' || sub === '-h' || sub === '--help') {
    ctx.stderr(TASK_HELP);
    return sub === undefined ? 2 : 0;
  }
  const rest = args.slice(1);
  switch (sub) {
    case 'new':
      return cmdTaskNewCli(rest, ctx);
    case 'ls':
      return cmdTaskLsCli(rest, ctx);
    case 'status':
      return cmdTaskStatusCli(rest, ctx);
    case 'done':
      return cmdTaskDoneCli(rest, ctx);
    case 'edit':
      return cmdTaskEditCli(rest, ctx);
    default:
      ctx.stderr(`coord task: unknown subcommand: ${sub}\n\n${TASK_HELP}`);
      return 2;
  }
}

function cmdTaskNewCli(args: readonly string[], ctx: CliContext): number {
  let title: string | undefined;
  let priority: string | undefined;
  const tags: string[] = [];
  let due: string | undefined;
  let fromMessage: string | undefined;
  let noEdit = false;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '--priority':
        priority = args[++i];
        break;
      case '--tag':
        tags.push(args[++i] ?? '');
        break;
      case '--due':
        due = args[++i];
        break;
      case '--from-message':
        fromMessage = args[++i];
        break;
      case '--no-edit':
        noEdit = true;
        break;
      case '-h':
      case '--help':
        ctx.stderr(TASK_HELP);
        return 0;
      default:
        if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
        if (title === undefined) title = a;
        else throw new Error(`unexpected arg: ${a}`);
    }
  }
  if (title === undefined) {
    throw new Error('coord task new requires a <title>');
  }
  const r = cmdTaskNew({
    title,
    ...(priority !== undefined && { priority }),
    ...(tags.length > 0 && { tags }),
    ...(due !== undefined && { due }),
    ...(fromMessage !== undefined && { fromMessage }),
    noEdit,
    env: ctx.env,
    coordRoot: ctx.coordRoot,
  });
  ctx.stdout(`${r.filename}\n`);
  return 0;
}

function cmdTaskLsCli(args: readonly string[], ctx: CliContext): number {
  let status: string | undefined;
  let tag: string | undefined;
  let priority: string | undefined;
  let json = false;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '--status':
        status = args[++i];
        break;
      case '--tag':
        tag = args[++i];
        break;
      case '--priority':
        priority = args[++i];
        break;
      case '--json':
        json = true;
        break;
      case '-h':
      case '--help':
        ctx.stderr(TASK_HELP);
        return 0;
      default:
        throw new Error(`unknown flag: ${a}`);
    }
  }
  const r = cmdTaskLs({
    ...(status !== undefined && { status }),
    ...(tag !== undefined && { tag }),
    ...(priority !== undefined && { priority }),
    env: ctx.env,
    coordRoot: ctx.coordRoot,
  });
  if (json) {
    ctx.stdout(`${JSON.stringify(r.items)}\n`);
    return 0;
  }
  for (const t of r.items) {
    ctx.stdout(
      `${t.filename}\t${t.status}\t${t.priority ?? '-'}\t${t.title}\n`
    );
  }
  return 0;
}

function cmdTaskStatusCli(args: readonly string[], ctx: CliContext): number {
  const positional: string[] = [];
  for (const a of args) {
    if (a === '-h' || a === '--help') {
      ctx.stderr(TASK_HELP);
      return 0;
    }
    if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
    positional.push(a);
  }
  if (positional.length !== 2) {
    throw new Error('coord task status requires <filename> <STATE>');
  }
  const r = cmdTaskStatus({
    filename: positional[0]!,
    state: positional[1]!,
    env: ctx.env,
    coordRoot: ctx.coordRoot,
  });
  ctx.stdout(
    `${r.identity}/${r.filename}: ${r.previousStatus} → ${r.newStatus}\n`
  );
  return 0;
}

function cmdTaskDoneCli(args: readonly string[], ctx: CliContext): number {
  const positional: string[] = [];
  for (const a of args) {
    if (a === '-h' || a === '--help') {
      ctx.stderr(TASK_HELP);
      return 0;
    }
    if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
    positional.push(a);
  }
  if (positional.length !== 1) {
    throw new Error('coord task done requires a <filename>');
  }
  const r = cmdTaskStatus({
    filename: positional[0]!,
    state: 'done',
    env: ctx.env,
    coordRoot: ctx.coordRoot,
  });
  ctx.stdout(
    `${r.identity}/${r.filename}: ${r.previousStatus} → ${r.newStatus}\n`
  );
  return 0;
}

function cmdTaskEditCli(args: readonly string[], ctx: CliContext): number {
  const positional: string[] = [];
  for (const a of args) {
    if (a === '-h' || a === '--help') {
      ctx.stderr(TASK_HELP);
      return 0;
    }
    if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
    positional.push(a);
  }
  if (positional.length !== 1) {
    throw new Error('coord task edit requires a <filename>');
  }
  cmdTaskEdit({
    filename: positional[0]!,
    env: ctx.env,
    coordRoot: ctx.coordRoot,
  });
  return 0;
}
