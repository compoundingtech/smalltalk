// commands/ls.ts — list inbox or archive entries.

import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';

import { statSync } from 'node:fs';

import {
  archiveDir,
  filenameTimestamp,
  inboxDir,
  parseFrontmatter,
  pluralize,
  prefixOf,
  resolveIdentity,
  validFilename,
  validOutsideFilename,
} from '../common.ts';

export interface LsInput {
  recipient?: string | undefined;
  archive?: boolean;
  /** Filename ts >= since (`<unix-ms>` prefix). Undefined means no filter. */
  since?: number | undefined;
  /** Match files whose `from:` frontmatter equals this. */
  fromFilter?: string | undefined;
  /**
   * When true, populate {@link LsResult.items} with parsed frontmatter
   * for each match. Off by default to preserve the cheap filename-only
   * path that `st ls` and `st watch` rely on.
   */
  withMeta?: boolean;
  /**
   * Issue #8: when true, list prefix-sibling attachments in the folder
   * whose `<unix-ms>-<rand6>` prefix has no matching `.md` in the same
   * folder. These are orphans — left behind when a `.md` is archived
   * without `--with-attachments`. Incompatible with {@link withMeta} and
   * {@link fromFilter} (no frontmatter to project / filter on).
   */
  orphans?: boolean;

  env: NodeJS.ProcessEnv;
  stRoot: string;
}

export interface LsItem {
  filename: string;
  /** Derived from the filename's `<unix-ms>` prefix; always present. */
  ts: number;
  from: string | null;
  subject: string | null;
  inReplyTo: string | null;
  tags: string[];
  priority: 'low' | 'normal' | 'high' | null;
}

export interface LsResult {
  /** Matching filenames in chronological (filename) order. */
  matches: string[];
  /** Header text for the human-readable form (`# N message[s] in inbox`). */
  header: string;
  /** Whether the lookup was against archive/ (vs inbox/). */
  archive: boolean;
  /**
   * Per-match parsed frontmatter. Present iff {@link LsInput.withMeta}
   * was true. Same length and order as {@link matches}.
   */
  items?: LsItem[];
}

function buildOutsideItem(filename: string, dir: string): LsItem {
  // Outside .md: no trustworthy frontmatter to project. Use file
  // mtime as the derived ts so callers can still sort by arrival
  // order (the LAYOUT-004 prefix isn't there to give it to us).
  let ts = 0;
  try {
    ts = statSync(join(dir, filename)).mtimeMs;
  } catch {
    // Race — file removed between readdir and stat. Leave ts at 0.
  }
  return {
    filename,
    ts,
    from: 'outside',
    subject: null,
    inReplyTo: null,
    tags: [],
    priority: null,
  };
}

function buildItem(filename: string, dir: string): LsItem {
  const text = readFileSync(join(dir, filename), 'utf8');
  const fm = parseFrontmatter(text).fm;
  const from = typeof fm.from === 'string' && fm.from.length > 0 ? fm.from : null;
  const subject =
    typeof fm.subject === 'string' && fm.subject.length > 0
      ? fm.subject
      : null;
  const inReplyTo =
    typeof fm['in-reply-to'] === 'string' && fm['in-reply-to'].length > 0
      ? fm['in-reply-to']
      : null;
  let tags: string[] = [];
  if (Array.isArray(fm.tags)) {
    tags = fm.tags.map((t) => String(t));
  } else if (typeof fm.tags === 'string' && fm.tags.length > 0) {
    // Stored shape is the raw `[a, b]` list scalar — split conservatively.
    tags = fm.tags
      .replace(/^\[/, '')
      .replace(/\]$/, '')
      .split(',')
      .map((t) => t.trim().replace(/^"(.*)"$/, '$1').replace(/^'(.*)'$/, '$1'))
      .filter((t) => t.length > 0);
  }
  const priorityRaw = typeof fm.priority === 'string' ? fm.priority : '';
  const priority =
    priorityRaw === 'low' || priorityRaw === 'normal' || priorityRaw === 'high'
      ? priorityRaw
      : null;
  return {
    filename,
    ts: filenameTimestamp(filename),
    from,
    subject,
    inReplyTo,
    tags,
    priority,
  };
}

export function cmdLs(input: LsInput): LsResult {
  // Lenient on explicit <other>: a peer's tree on this machine
  // often has only inbox/ from a one-shot send, not archive/. The
  // existsSync gate below treats a missing folder as empty.
  const recipient = resolveIdentity({
    explicit: input.recipient,
    env: input.env,
    stRoot: input.stRoot,
    ...(input.recipient ? { policy: 'lenient' as const } : {}),
  });
  const archive = input.archive === true;
  const dir = archive
    ? archiveDir(recipient, input.stRoot)
    : inboxDir(recipient, input.stRoot);
  const label = archive ? 'archive' : 'inbox';

  if (!existsSync(dir)) {
    const empty: LsResult = {
      matches: [],
      header: `# 0 messages in ${label}`,
      archive,
    };
    if (input.withMeta === true) empty.items = [];
    return empty;
  }

  const since = input.since;
  const fromFilter = input.fromFilter;
  const withMeta = input.withMeta === true;
  const orphansMode = input.orphans === true;

  let names: string[];
  try {
    names = readdirSync(dir);
  } catch {
    // Permission-denied (chmod 000), I/O error, etc — degrade
    // gracefully per the brief-016 read-side resilience contract.
    const empty: LsResult = {
      matches: [],
      header: orphansMode
        ? `# 0 orphan attachments in ${label}`
        : `# 0 messages in ${label}`,
      archive,
    };
    if (withMeta) empty.items = [];
    return empty;
  }

  if (orphansMode) {
    // Build the set of canonical .md prefixes present in this folder;
    // a prefix-sibling whose prefix is NOT in this set is an orphan.
    const mdPrefixes = new Set<string>();
    for (const name of names) {
      if (validFilename(name)) mdPrefixes.add(name.slice(0, 20));
    }
    const orphans: string[] = [];
    for (const name of names.sort()) {
      if (validFilename(name)) continue; // it's a real .md, not an orphan
      const pre = prefixOf(name);
      if (pre === null) continue; // random file, not a coord sibling
      if (mdPrefixes.has(pre)) continue; // has a matching .md; not orphaned
      if (since !== undefined) {
        // The 13-digit prefix segment IS the unix-ms timestamp; reuse it
        // rather than calling filenameTimestamp (which validates .md grammar).
        if (Number(pre.slice(0, 13)) < since) continue;
      }
      orphans.push(name);
    }
    const n = orphans.length;
    const word = pluralize(n, 'attachment', 'attachments');
    const header = `# ${n} orphan ${word} in ${label}`;
    return { matches: orphans, header, archive };
  }

  const matches: string[] = [];
  const items: LsItem[] = [];
  for (const name of names.sort()) {
    const canonical = validFilename(name);
    const outside = !canonical && validOutsideFilename(name);
    if (!canonical && !outside) continue;
    if (since !== undefined) {
      // Outside filenames have no LAYOUT prefix; use mtime as the
      // derived arrival time so `--since` treats them consistently.
      let nameTs: number;
      if (canonical) {
        nameTs = filenameTimestamp(name);
      } else {
        try {
          nameTs = statSync(join(dir, name)).mtimeMs;
        } catch {
          nameTs = 0;
        }
      }
      if (nameTs < since) continue;
    }
    if (fromFilter !== undefined && fromFilter !== '') {
      if (outside) {
        // `fromFilter` is compared against `outside` as a marker so
        // callers can specifically pull outside messages. Any other
        // filter value can't match — outside files have no trusted
        // sender identity.
        if (fromFilter !== 'outside') continue;
      } else {
        const text = readFileSync(join(dir, name), 'utf8');
        const fm = parseFrontmatter(text).fm;
        if (fm.from !== fromFilter) continue;
      }
    }
    matches.push(name);
    if (withMeta) {
      items.push(outside ? buildOutsideItem(name, dir) : buildItem(name, dir));
    }
  }

  const n = matches.length;
  const header = `# ${n} ${pluralize(n, 'message', 'messages')} in ${label}`;

  const result: LsResult = { matches, header, archive };
  if (withMeta) result.items = items;
  return result;
}

export { cmdLs as cmdLsCore };

// ─── CLI wrapper ────────────────────────────────────────────────────────

import { invokedName, type CliContext } from '../cli-context.ts';

export function cmdLsCli(args: readonly string[], ctx: CliContext): number {
  let recipient: string | undefined;
  let archive = false;
  let count = false;
  let json = false;
  let orphans = false;
  let since: number | undefined;
  let fromFilter: string | undefined;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '--archive':
        archive = true;
        break;
      case '--count':
        count = true;
        break;
      case '--json':
        json = true;
        break;
      case '--orphans':
        orphans = true;
        break;
      case '--since': {
        const v = args[++i];
        if (v === undefined || !/^[0-9]+$/.test(v)) {
          throw new Error('--since must be a unix-ms integer');
        }
        since = Number(v);
        break;
      }
      case '--from':
        fromFilter = args[++i];
        break;
      case '-h':
      case '--help':
        ctx.stderr(
          `usage: ${invokedName(ctx.env)} message ls [<recipient>] [--archive] [--count|--json] [--since UNIX_MS] [--from ID] [--orphans]\n`
        );
        return 0;
      default:
        if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
        if (recipient === undefined) recipient = a;
        else throw new Error(`unexpected arg: ${a}`);
    }
  }
  if (count && json) {
    throw new Error('--count and --json are mutually exclusive');
  }
  if (orphans && fromFilter !== undefined) {
    throw new Error('--orphans and --from are mutually exclusive (orphans have no frontmatter)');
  }
  const r = cmdLs({
    ...(recipient !== undefined && { recipient }),
    archive,
    ...(since !== undefined && { since }),
    ...(fromFilter !== undefined && { fromFilter }),
    // --orphans skips message-parsing entirely; --json on orphans emits a
    // filename-only list (see below), so withMeta stays off in that case.
    withMeta: json && !orphans,
    orphans,
    env: ctx.env,
    stRoot: ctx.stRoot,
  });
  if (count) {
    ctx.stdout(`${r.matches.length}\n`);
    return 0;
  }
  if (json) {
    if (orphans) {
      // Richer shape than bare filenames: include the derived ts so
      // consumers can age-sort without re-parsing the prefix.
      const items = r.matches.map((filename) => ({
        filename,
        ts: Number(filename.slice(0, 13)),
      }));
      ctx.stdout(`${JSON.stringify(items)}\n`);
      return 0;
    }
    ctx.stdout(`${JSON.stringify(r.items ?? [])}\n`);
    return 0;
  }
  ctx.stderr(`${r.header}\n`);
  for (const name of r.matches) ctx.stdout(`${name}\n`);
  return 0;
}
