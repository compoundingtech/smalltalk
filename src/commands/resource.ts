// commands/resource.ts — manage an identity's annotated-URL resources.
//
// brief-009 item 5: `<identity>/resources/` is the third optional folder
// under each identity. Each file is `<unix-ms>-<rand6>.md` with the URL
// in YAML frontmatter and an optional description in the body. Mutable
// only by the owning identity; peers read via sync. Mirrors `tasks/`'s
// single-writer convention from brief-015 (now removed).
//
// Subcommands (each pairs a typed core function with a CLI wrapper):
//   st resource add <url> [--title T] [--tag T,T] [--relation REL] [--body-stdin]
//   st resource ls [<identity>]
//   st resource read [<identity>] <filename>
//   st resource rm <filename>

import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  unlinkSync,
} from 'node:fs';
import { join } from 'node:path';

import { invokedName, type CliContext } from '../cli-context.ts';
import {
  emitFrontmatter,
  genFilename,
  parseFrontmatter,
  resolveIdentity,
  resourcesDir,
  safeAtomicWrite,
  validFilename,
} from '../common.ts';
import {
  InvalidFilenameError,
  InvalidResourceUrlError,
  ResourceNotFoundError,
} from '../errors.ts';

// ─── Shape ──────────────────────────────────────────────────────────────

export interface ResourceRecord {
  filename: string;
  url: string;
  title: string | null;
  tags: string[];
  /** Optional, free-form. Canonical (non-enforced) values:
   *  `owns` / `relates-to` / `depends-on`. Never inferred. `null` when
   *  the frontmatter key is absent — the bare URL stays first-class. */
  relation: string | null;
  body: string;
}

// ─── Helpers ────────────────────────────────────────────────────────────

/**
 * Validate that `url` has the basic shape of a URL: lowercase scheme,
 * `://`, and at least one character after. We're deliberately lenient
 * — the brief calls for arbitrary URL-shaped strings (https://,
 * pty://, anything an agent invents). No host validation, no path
 * parsing.
 */
const URL_RE = /^[a-z][a-z0-9+.-]*:\/\/.+/;
function validResourceUrl(s: string): boolean {
  if (typeof s !== 'string') return false;
  if (s.length === 0) return false;
  return URL_RE.test(s);
}

function readResourceFile(path: string, filename: string): ResourceRecord {
  const text = readFileSync(path, 'utf8');
  const { fm, body } = parseFrontmatter(text);
  const url = typeof fm.url === 'string' ? fm.url : '';
  const title =
    typeof fm.title === 'string' && fm.title.length > 0 ? fm.title : null;
  let tags: string[] = [];
  if (Array.isArray(fm.tags)) {
    tags = fm.tags.map((t) => String(t));
  } else if (typeof fm.tags === 'string' && fm.tags.length > 0) {
    tags = fm.tags
      .replace(/^\[/, '')
      .replace(/\]$/, '')
      .split(',')
      .map((t) =>
        t
          .trim()
          .replace(/^"(.*)"$/, '$1')
          .replace(/^'(.*)'$/, '$1')
      )
      .filter((t) => t.length > 0);
  }
  // `relation` is free-form. Absent → null. Empty string → null.
  const relation =
    typeof fm.relation === 'string' && fm.relation.length > 0
      ? fm.relation
      : null;
  return { filename, url, title, tags, relation, body };
}

function normalizeTags(input: string | string[] | undefined): string[] {
  if (input === undefined) return [];
  const raw = Array.isArray(input)
    ? input.flatMap((s) => s.split(','))
    : input.split(',');
  return raw.map((t) => t.trim()).filter((t) => t.length > 0);
}

function ensureResourcesDir(identity: string, root: string): string {
  const dir = resourcesDir(identity, root);
  mkdirSync(dir, { recursive: true });
  return dir;
}

// ─── st resource add ─────────────────────────────────────────────────

export interface ResourceAddInput {
  url: string;
  title?: string | undefined;
  tags?: string | string[] | undefined;
  /** Optional free-form relation between the agent and the URL. Canonical
   *  values: `owns` / `relates-to` / `depends-on`. Never inferred —
   *  absent means "no claim about the relationship; the bare URL
   *  stands". */
  relation?: string | undefined;
  /** Optional body (description). Frontmatter is built from
   *  url/title/tags/relation. */
  body?: string | undefined;
  /** Override the owning identity. Defaults to $ST_AGENT. */
  identity?: string | undefined;
  env: NodeJS.ProcessEnv;
  stRoot: string;
}

export interface ResourceAddResult {
  filename: string;
  path: string;
  identity: string;
}

export function cmdResourceAdd(input: ResourceAddInput): ResourceAddResult {
  if (!validResourceUrl(input.url)) {
    throw new InvalidResourceUrlError(input.url);
  }
  const identity = resolveIdentity({
    ...(input.identity !== undefined && { explicit: input.identity }),
    env: input.env,
    stRoot: input.stRoot,
  });
  const dir = ensureResourcesDir(identity, input.stRoot);
  const filename = genFilename();
  const path = join(dir, filename);

  const fm: Record<string, unknown> = { url: input.url };
  if (input.title !== undefined && input.title.length > 0) {
    fm.title = input.title;
  }
  const tags = normalizeTags(input.tags);
  if (tags.length > 0) fm.tags = tags;
  if (input.relation !== undefined && input.relation.length > 0) {
    fm.relation = input.relation;
  }

  const body = input.body !== undefined ? input.body : '';
  const content = emitFrontmatter(fm, body);
  safeAtomicWrite(path, content);

  return { filename, path, identity };
}

// ─── st resource ls ──────────────────────────────────────────────────

export interface ResourceLsInput {
  /** Whose resources to list. Defaults to $ST_AGENT. */
  identity?: string | undefined;
  env: NodeJS.ProcessEnv;
  stRoot: string;
}

export interface ResourceLsResult {
  identity: string;
  matches: string[];
}

export function cmdResourceLs(input: ResourceLsInput): ResourceLsResult {
  const identity = resolveIdentity({
    ...(input.identity !== undefined && { explicit: input.identity }),
    env: input.env,
    stRoot: input.stRoot,
    // Read-side: don't require the inbox/archive skeleton — peers may have
    // only resources/ for an identity in some edge cases.
    policy: 'lenient',
  });
  const dir = resourcesDir(identity, input.stRoot);
  if (!existsSync(dir)) return { identity, matches: [] };
  let names: string[];
  try {
    names = readdirSync(dir);
  } catch {
    return { identity, matches: [] };
  }
  const matches = names.filter((n) => validFilename(n)).sort();
  return { identity, matches };
}

/**
 * Walk an identity's resources/ folder and return parsed records. Used
 * by the SDK's `st.resources.list` and the MCP tool. No
 * resolveIdentity — the caller passes the identity directly.
 */
export function listResourceRecords(
  identity: string,
  root: string
): ResourceRecord[] {
  const dir = resourcesDir(identity, root);
  if (!existsSync(dir)) return [];
  let names: string[];
  try {
    names = readdirSync(dir);
  } catch {
    return [];
  }
  const items: ResourceRecord[] = [];
  for (const name of names.sort()) {
    if (!validFilename(name)) continue;
    try {
      items.push(readResourceFile(join(dir, name), name));
    } catch {
      continue;
    }
  }
  return items;
}

// ─── st resource read ────────────────────────────────────────────────

export interface ResourceReadInput {
  /** Whose resource to read. Defaults to $ST_AGENT. */
  identity?: string | undefined;
  filename: string;
  env: NodeJS.ProcessEnv;
  stRoot: string;
}

export interface ResourceReadResult {
  identity: string;
  record: ResourceRecord;
}

export function cmdResourceRead(input: ResourceReadInput): ResourceReadResult {
  if (!validFilename(input.filename)) {
    throw new InvalidFilenameError(input.filename);
  }
  const identity = resolveIdentity({
    ...(input.identity !== undefined && { explicit: input.identity }),
    env: input.env,
    stRoot: input.stRoot,
    policy: 'lenient',
  });
  const path = join(resourcesDir(identity, input.stRoot), input.filename);
  if (!existsSync(path)) {
    throw new ResourceNotFoundError(identity, input.filename);
  }
  return { identity, record: readResourceFile(path, input.filename) };
}

// ─── st resource rm ──────────────────────────────────────────────────

export interface ResourceRemoveInput {
  /** Defaults to $ST_AGENT. rm only operates on the OWNER's
   * own resources — peers' resources are read-only by the LAYOUT
   * single-writer rule. */
  identity?: string | undefined;
  filename: string;
  env: NodeJS.ProcessEnv;
  stRoot: string;
}

export interface ResourceRemoveResult {
  identity: string;
  filename: string;
  removed: boolean;
}

export function cmdResourceRemove(
  input: ResourceRemoveInput
): ResourceRemoveResult {
  if (!validFilename(input.filename)) {
    throw new InvalidFilenameError(input.filename);
  }
  const identity = resolveIdentity({
    ...(input.identity !== undefined && { explicit: input.identity }),
    env: input.env,
    stRoot: input.stRoot,
  });
  const path = join(resourcesDir(identity, input.stRoot), input.filename);
  if (!existsSync(path)) {
    throw new ResourceNotFoundError(identity, input.filename);
  }
  unlinkSync(path);
  return { identity, filename: input.filename, removed: true };
}

// ─── CLI wrapper ────────────────────────────────────────────────────────

function resourceHelp(name: string): string {
  return (
    `usage: ${name} resource <subcommand> [args...]\n\n` +
    '  add <url> [--title T] [--tag T,T] [--relation REL] [--body-stdin]\n' +
    '  ls [<identity>]\n' +
    '  read [<identity>] <filename>\n' +
    '  rm <filename>\n\n' +
    '  Resources live at $ST_ROOT/<identity>/resources/.\n' +
    '  Each file is <unix-ms>-<rand6>.md with `url:` in frontmatter\n' +
    '  and an optional description in the body. Single-writer: only\n' +
    '  the identity owner writes; peers read via sync.\n\n' +
    '  --relation REL  Optional, free-form. Canonical (non-enforced)\n' +
    '                  values: `owns`, `relates-to`, `depends-on`.\n' +
    '                  Never inferred; absent by default. The bare\n' +
    '                  URL is first-class with or without it.\n\n' +
    '  Examples:\n' +
    `    ${name} resource add https://github.com/org/repo/pull/9 --title "the PR" --tag repo\n` +
    `    ${name} resource ls bob     # what URLs has bob published?\n`
  );
}

export async function cmdResourceCli(
  args: readonly string[],
  ctx: CliContext
): Promise<number> {
  const name = invokedName(ctx.env);
  const sub = args[0];
  if (sub === undefined || sub === 'help' || sub === '-h' || sub === '--help') {
    ctx.stderr(resourceHelp(name));
    return sub === undefined ? 2 : 0;
  }
  const rest = args.slice(1);
  switch (sub) {
    case 'add':
      return await cmdResourceAddCli(rest, ctx);
    case 'ls':
      return cmdResourceLsCli(rest, ctx);
    case 'read':
      return cmdResourceReadCli(rest, ctx);
    case 'rm':
    case 'remove':
      return cmdResourceRemoveCli(rest, ctx);
    default:
      ctx.stderr(
        `${name} resource: unknown subcommand: ${sub}\n\n${resourceHelp(name)}`
      );
      return 2;
  }
}

async function cmdResourceAddCli(
  args: readonly string[],
  ctx: CliContext
): Promise<number> {
  let url: string | undefined;
  let title: string | undefined;
  const tags: string[] = [];
  let relation: string | undefined;
  let bodyStdin = false;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '--title':
        title = args[++i];
        break;
      case '--tag':
        tags.push(args[++i] ?? '');
        break;
      case '--relation':
        relation = args[++i];
        break;
      case '--body-stdin':
        bodyStdin = true;
        break;
      case '-h':
      case '--help':
        ctx.stderr(resourceHelp(invokedName(ctx.env)));
        return 0;
      default:
        if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
        if (url === undefined) url = a;
        else throw new Error(`unexpected arg: ${a}`);
    }
  }
  if (url === undefined) {
    throw new Error('st resource add requires a <url>');
  }
  let body: string | undefined;
  if (bodyStdin) {
    const buf = await ctx.readStdin();
    body = buf.toString('utf8');
  }
  const r = cmdResourceAdd({
    url,
    ...(title !== undefined && { title }),
    ...(tags.length > 0 && { tags }),
    ...(relation !== undefined && { relation }),
    ...(body !== undefined && { body }),
    env: ctx.env,
    stRoot: ctx.stRoot,
  });
  ctx.stdout(`${r.filename}\n`);
  return 0;
}

function cmdResourceLsCli(
  args: readonly string[],
  ctx: CliContext
): number {
  let identity: string | undefined;
  let json = false;
  for (const a of args) {
    if (a === '-h' || a === '--help') {
      ctx.stderr(resourceHelp(invokedName(ctx.env)));
      return 0;
    }
    if (a === '--json') {
      json = true;
      continue;
    }
    if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
    if (identity === undefined) identity = a;
    else throw new Error(`unexpected arg: ${a}`);
  }
  const r = cmdResourceLs({
    ...(identity !== undefined && { identity }),
    env: ctx.env,
    stRoot: ctx.stRoot,
  });
  if (json) {
    ctx.stdout(`${JSON.stringify(r.matches)}\n`);
    return 0;
  }
  for (const fn of r.matches) ctx.stdout(`${fn}\n`);
  return 0;
}

function cmdResourceReadCli(
  args: readonly string[],
  ctx: CliContext
): number {
  let json = false;
  const positional: string[] = [];
  for (const a of args) {
    if (a === '-h' || a === '--help') {
      ctx.stderr(resourceHelp(invokedName(ctx.env)));
      return 0;
    }
    if (a === '--json') {
      json = true;
      continue;
    }
    if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
    positional.push(a);
  }
  let identity: string | undefined;
  let filename: string;
  if (positional.length === 1) {
    filename = positional[0]!;
  } else if (positional.length === 2) {
    identity = positional[0];
    filename = positional[1]!;
  } else {
    throw new Error('st resource read requires [<identity>] <filename>');
  }
  const r = cmdResourceRead({
    ...(identity !== undefined && { identity }),
    filename,
    env: ctx.env,
    stRoot: ctx.stRoot,
  });
  if (json) {
    ctx.stdout(
      `${JSON.stringify({
        identity: r.identity,
        filename: r.record.filename,
        url: r.record.url,
        title: r.record.title,
        tags: r.record.tags,
        relation: r.record.relation,
        body: r.record.body,
      })}\n`
    );
    return 0;
  }
  ctx.stdout(`url: ${r.record.url}\n`);
  if (r.record.title !== null) ctx.stdout(`title: ${r.record.title}\n`);
  if (r.record.tags.length > 0) {
    ctx.stdout(`tags: ${r.record.tags.join(', ')}\n`);
  }
  if (r.record.relation !== null) {
    ctx.stdout(`relation: ${r.record.relation}\n`);
  }
  if (r.record.body.length > 0) {
    ctx.stdout('\n');
    ctx.stdout(r.record.body);
    if (!r.record.body.endsWith('\n')) ctx.stdout('\n');
  }
  return 0;
}

function cmdResourceRemoveCli(
  args: readonly string[],
  ctx: CliContext
): number {
  const positional: string[] = [];
  for (const a of args) {
    if (a === '-h' || a === '--help') {
      ctx.stderr(resourceHelp(invokedName(ctx.env)));
      return 0;
    }
    if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
    positional.push(a);
  }
  if (positional.length !== 1) {
    throw new Error('st resource rm requires a <filename>');
  }
  const r = cmdResourceRemove({
    filename: positional[0]!,
    env: ctx.env,
    stRoot: ctx.stRoot,
  });
  ctx.stdout(`${r.identity}/${r.filename}: removed\n`);
  return 0;
}
