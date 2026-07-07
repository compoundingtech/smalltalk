// commands/read.ts — print one message.

import { existsSync, readFileSync, rmSync } from 'node:fs';

import {
  archiveDir,
  inboxDir,
  parseFrontmatter,
  resolveIdentity,
  validDeliverableFilename,
  validFilename,
} from '../common.ts';
import {
  InvalidFilenameError,
  MessageNotFoundError,
} from '../errors.ts';

export interface ReadInput {
  recipient?: string | undefined;
  filename: string;
  raw?: boolean;
  /** Prefer archive/ first; auto-fallback to inbox if not in archive. */
  fromArchive?: boolean;

  env: NodeJS.ProcessEnv;
  stRoot: string;
}

export interface ReadResult {
  /** The body text. In raw mode, the entire file (frontmatter + body). */
  body: string;
  /** Multi-line header (formatted mode). Empty in raw mode. */
  header: string;
  /** Which folder the file was found in. */
  label: 'inbox' | 'archive';
  /** Absolute path to the file that was read. */
  path: string;
  /** True if the file lacks parseable frontmatter. */
  untyped: boolean;
  /** Resolved recipient identity (derived from the file's path). */
  recipient: string;
  /** Parsed frontmatter map. Empty in raw mode and on untyped files. */
  fm: Record<string, unknown>;
}

export function cmdRead(input: ReadInput): ReadResult {
  if (!input.filename) throw new Error('<filename> required');

  // Lenient on explicit <other>: peer trees on this machine may be
  // partial (inbox/ from a one-shot send without archive/). The
  // inbox/archive existsSync paths below tolerate missing dirs.
  const recipient = resolveIdentity({
    explicit: input.recipient,
    env: input.env,
    stRoot: input.stRoot,
    ...(input.recipient ? { policy: 'lenient' as const } : {}),
  });
  if (!validDeliverableFilename(input.filename)) {
    throw new InvalidFilenameError(input.filename);
  }
  const isOutside = !validFilename(input.filename);

  const inboxPath = `${inboxDir(recipient, input.stRoot)}/${input.filename}`;
  const archivePath = `${archiveDir(recipient, input.stRoot)}/${input.filename}`;

  let path: string;
  let label: 'inbox' | 'archive';
  if (input.fromArchive && existsSync(archivePath)) {
    path = archivePath;
    label = 'archive';
  } else if (existsSync(inboxPath)) {
    path = inboxPath;
    label = 'inbox';
  } else if (existsSync(archivePath)) {
    path = archivePath;
    label = 'archive';
  } else {
    throw new MessageNotFoundError(recipient, input.filename);
  }

  // Lazy-read sweep: if we resolved to inbox and a byte-identical
  // archive twin exists, the inbox copy is a zombie (archive-as-
  // tombstone invariant). Remove the inbox copy now and read the
  // archive copy instead. Bounded work — one stat + at most one
  // byte-compare per read. Bytes differ? Leave both alone; that's
  // an unrelated message authored under a colliding filename, not a
  // twin. Errors during compare/remove are non-fatal; the read
  // proceeds with the inbox copy we already found.
  if (label === 'inbox' && existsSync(archivePath)) {
    try {
      const inboxBuf = readFileSync(path);
      const archiveBuf = readFileSync(archivePath);
      if (inboxBuf.equals(archiveBuf)) {
        rmSync(path);
        path = archivePath;
        label = 'archive';
      }
    } catch {
      // leave both alone on error; the original path/label still works
    }
  }

  const text = readFileSync(path, 'utf8');

  if (input.raw === true) {
    return {
      body: text,
      header: '',
      label,
      path,
      untyped: false,
      recipient,
      fm: {},
    };
  }

  // Off-format `.md` files: return the whole file as body regardless
  // of whether it happens to have frontmatter. We can't trust the
  // sender's claimed identity through an unofficial filename, so
  // `fm` is empty and the file reads as an "outside" message.
  if (isOutside) {
    const header = `# ${label}/${input.filename} (outside: non-canonical filename)\n`;
    return {
      body: text,
      header,
      label,
      path,
      untyped: true,
      recipient,
      fm: {},
    };
  }

  const parsed = parseFrontmatter(text);
  const hasFm = textHasFrontmatter(text);
  if (!hasFm) {
    const header = `# ${label}/${input.filename} (untyped: no frontmatter)\n`;
    return {
      body: text,
      header,
      label,
      path,
      untyped: true,
      recipient,
      fm: {},
    };
  }

  const ts = input.filename.split('-')[0]!;
  const lines: string[] = [];
  lines.push(`# ${label}/${input.filename}`);
  lines.push(`to:          ${recipient}  (derived from path)`);
  lines.push(`ts:          ${ts}  (derived from filename)`);
  for (const key of ['from', 'subject', 'in-reply-to', 'tags', 'priority']) {
    const v = parsed.fm[key];
    if (typeof v === 'string' && v.length > 0) {
      lines.push(formatHeaderRow(key, v));
    }
  }
  const header = lines.join('\n') + '\n';

  return {
    body: parsed.body,
    header,
    label,
    path,
    untyped: false,
    recipient,
    fm: parsed.fm,
  };
}

/**
 * Build the structured shape emitted by `st message read --json`.
 *
 * Mirrors the {@link Coord.read} / `coord_msg_read` MCP tool projection so
 * a programmatic consumer can use one parser across both surfaces.
 * Untyped messages (no frontmatter) get `message.from = ""`, matching the
 * lib.ts permissive shape.
 */
export interface ReadJsonShape {
  filename: string;
  identity: string;
  folder: 'inbox' | 'archive';
  message: {
    from: string;
    subject?: string;
    inReplyTo?: string;
    tags?: string[];
    priority?: 'low' | 'normal' | 'high';
    body: string;
  };
}

export function buildReadJsonShape(
  filename: string,
  r: ReadResult
): ReadJsonShape {
  const fm = r.fm;
  const fromRaw = typeof fm.from === 'string' ? fm.from : '';
  const message: ReadJsonShape['message'] = { from: fromRaw, body: r.body };
  if (typeof fm.subject === 'string' && fm.subject.length > 0) {
    message.subject = fm.subject;
  }
  if (typeof fm['in-reply-to'] === 'string' && fm['in-reply-to'].length > 0) {
    message.inReplyTo = fm['in-reply-to'];
  }
  // Tags: stored as either the raw "[a, b]" scalar (parseFrontmatter
  // returns the unwrapped string) or — under the array-emit path in
  // emitFrontmatter — as a real string[]. Project both to string[].
  if (Array.isArray(fm.tags)) {
    message.tags = fm.tags.map((t) => String(t));
  } else if (typeof fm.tags === 'string' && fm.tags.length > 0) {
    const trimmed = fm.tags.replace(/^\[/, '').replace(/\]$/, '');
    const tags = trimmed
      .split(',')
      .map((t) => t.trim().replace(/^"(.*)"$/, '$1').replace(/^'(.*)'$/, '$1'))
      .filter((t) => t.length > 0);
    if (tags.length > 0) message.tags = tags;
  }
  if (
    fm.priority === 'low' ||
    fm.priority === 'normal' ||
    fm.priority === 'high'
  ) {
    message.priority = fm.priority;
  }
  return { filename, identity: r.recipient, folder: r.label, message };
}

/**
 * Disambiguate "untyped, no fences" from "valid frontmatter, all keys
 * empty" so the formatter renders the right shape. parseFrontmatter
 * already returns `{ fm: {}, body: text }` for both cases, so we re-check
 * the raw text here.
 */
function textHasFrontmatter(text: string): boolean {
  const lines = text.split('\n');
  if (lines.length === 0 || lines[0] !== '---') return false;
  for (let i = 1; i < lines.length; i++) {
    if (lines[i] === '---') return true;
  }
  return false;
}

const HEADER_PAD = 'in-reply-to:'.length; // longest label prefix
function formatHeaderRow(key: string, value: string): string {
  const label = `${key}:`.padEnd(HEADER_PAD, ' ');
  return `${label} ${value}`;
}

/**
 * Resolve a positional shape that may be `[<filename>]` or
 * `[<identity>, <filename>]`.
 *
 * The single-positional case uses the `.md` suffix to disambiguate:
 * identity names can't contain `.` per LAYOUT-004, so any positional
 * ending in `.md` is a filename. This lets a typo like
 * `st message read nope.md` reach the cmdRead filename validator
 * (which surfaces a clear InvalidFilenameError) instead of being
 * mis-parsed as the optional identity and bailing with the
 * misleading "<filename> required" message.
 */
export function splitReadPositionals(
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

export { cmdRead as cmdReadCore };

// ─── CLI wrapper ────────────────────────────────────────────────────────

import { invokedName, type CliContext } from '../cli-context.ts';

export function cmdReadCli(args: readonly string[], ctx: CliContext): number {
  let raw = false;
  let fromArchive = false;
  let json = false;
  const positional: string[] = [];
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '--raw':
        raw = true;
        break;
      case '--archive':
        fromArchive = true;
        break;
      case '--json':
        json = true;
        break;
      case '-h':
      case '--help':
        ctx.stderr(
          `usage: ${invokedName(ctx.env)} message read [<identity>] <filename> [--raw|--json] [--archive]\n`
        );
        return 0;
      default:
        if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
        positional.push(a);
    }
  }
  if (raw && json) {
    throw new Error('--raw and --json are mutually exclusive');
  }
  const { recipient, filename } = splitReadPositionals(positional);
  if (filename === undefined) throw new Error('<filename> required');
  const r = cmdRead({
    ...(recipient !== undefined && { recipient }),
    filename,
    raw,
    fromArchive,
    env: ctx.env,
    stRoot: ctx.stRoot,
  });
  if (raw) {
    ctx.stdout(r.body);
    return 0;
  }
  if (json) {
    ctx.stdout(`${JSON.stringify(buildReadJsonShape(filename, r))}\n`);
    return 0;
  }
  ctx.stderr(r.header);
  if (!r.untyped) ctx.stderr('\n');
  ctx.stdout(r.body);
  return 0;
}
