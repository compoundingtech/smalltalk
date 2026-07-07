// commands/archive.ts — `st archive [<id>] <filename>` and `st archive trim ...`

import { existsSync, readFileSync, readdirSync, renameSync, rmSync } from 'node:fs';
import { join } from 'node:path';

import {
  archiveDir,
  ensureIdentityDirs,
  filenameTimestamp,
  inboxDir,
  msNow,
  pluralize,
  prefixOf,
  resolveIdentity,
  validDeliverableFilename,
  validFilename,
} from '../common.ts';
import {
  ArchiveConflictError,
  InvalidDurationError,
  InvalidFilenameError,
  MessageNotFoundError,
} from '../errors.ts';

// ─── archive (move inbox → archive) ─────────────────────────────────────

export interface ArchiveInput {
  recipient?: string | undefined;
  filename: string;
  /**
   * Issue #8: when true, also move prefix-sibling attachments — every
   * file in inbox/ whose `<unix-ms>-<rand6>` prefix matches the canonical
   * `.md`. Default false preserves the LAYOUT-004 "st owns only the
   * .md" semantic; opt-in keeps attachments lifecycle-coupled when the
   * caller wants that.
   */
  withAttachments?: boolean;
  env: NodeJS.ProcessEnv;
  stRoot: string;
}

export type ArchiveOutcome =
  | { kind: 'moved'; message: 'archived' }
  | {
      kind: 'idempotent';
      message: 'archived (idempotent: archive copy already present)';
    };

/** One archived attachment sibling — paired with its outcome. */
export interface ArchivedAttachment {
  filename: string;
  outcome: ArchiveOutcome;
}

export interface ArchiveResult {
  outcome: ArchiveOutcome;
  recipient: string;
  /**
   * When {@link ArchiveInput.withAttachments} was true, the per-sibling
   * outcomes (excluding the canonical `.md`). Empty when no siblings
   * existed. Undefined when withAttachments was false.
   */
  attachments?: ArchivedAttachment[];
}

export function cmdArchive(input: ArchiveInput): ArchiveResult {
  if (!input.filename) throw new Error('<filename> required');

  const recipient = resolveIdentity({
    explicit: input.recipient,
    env: input.env,
    stRoot: input.stRoot,
  });
  if (!validDeliverableFilename(input.filename)) {
    throw new InvalidFilenameError(input.filename);
  }
  const isOutside = !validFilename(input.filename);

  const inbox = inboxDir(recipient, input.stRoot);
  const archive = archiveDir(recipient, input.stRoot);
  const ipath = join(inbox, input.filename);
  const apath = join(archive, input.filename);
  // Outside .md files have no LAYOUT prefix — no siblings to sweep.
  // Silently ignore --with-attachments in that case rather than
  // scanning for a non-existent prefix family.
  const withAttachments = input.withAttachments === true && !isOutside;

  // ─── Discover attachment siblings (issue #8) ────────────────────────
  // Find every file in inbox/ AND archive/ that shares the canonical
  // prefix (excluding the `.md` itself). We must look at archive too:
  // post-sync, a sibling may exist only in archive (peer already moved
  // it), or in both (matching the `.md` byte-identical case).
  const prefix = input.filename.slice(0, 20);
  const siblings: string[] = [];
  if (withAttachments) {
    const seen = new Set<string>();
    for (const dir of [inbox, archive]) {
      let entries: string[];
      try {
        entries = readdirSync(dir);
      } catch {
        continue;
      }
      for (const name of entries) {
        if (name === input.filename) continue;
        if (prefixOf(name) !== prefix) continue;
        if (seen.has(name)) continue;
        seen.add(name);
        siblings.push(name);
      }
    }
    siblings.sort();
  }

  // ─── Pre-flight conflict check ──────────────────────────────────────
  // Atomic semantics: if ANY family member would conflict (divergent
  // archive twin), refuse the whole operation rather than half-moving.
  // The `.md` itself is checked first; siblings only matter when
  // withAttachments is on.
  const familyCheck: { filename: string; ipath: string; apath: string }[] = [
    { filename: input.filename, ipath, apath },
  ];
  for (const s of siblings) {
    familyCheck.push({
      filename: s,
      ipath: join(inbox, s),
      apath: join(archive, s),
    });
  }
  for (const fam of familyCheck) {
    if (existsSync(fam.ipath) && existsSync(fam.apath)) {
      const ibuf = readFileSync(fam.ipath);
      const abuf = readFileSync(fam.apath);
      if (!ibuf.equals(abuf)) {
        throw new ArchiveConflictError(recipient, fam.filename);
      }
    }
  }

  // ─── Canonical .md outcome ──────────────────────────────────────────
  let canonical: ArchiveOutcome;
  if (!existsSync(ipath) && existsSync(apath)) {
    // Case 0 (post-sweep idempotent).
    canonical = idempotentOutcome();
  } else if (!existsSync(ipath)) {
    // Case 1: not in either folder.
    throw new MessageNotFoundError(recipient, input.filename);
  } else {
    ensureIdentityDirs(recipient, input.stRoot);
    if (existsSync(apath)) {
      // Case 2 — byte-identical already validated upstream.
      rmSync(ipath);
      canonical = idempotentOutcome();
    } else {
      // Case 4: clean rename.
      renameSync(ipath, apath);
      canonical = { kind: 'moved', message: 'archived' };
    }
  }

  if (!withAttachments) {
    return { outcome: canonical, recipient };
  }

  // ─── Sibling outcomes ───────────────────────────────────────────────
  const attachmentOutcomes: ArchivedAttachment[] = [];
  for (const s of siblings) {
    const sip = join(inbox, s);
    const sap = join(archive, s);
    let outcome: ArchiveOutcome;
    if (!existsSync(sip) && existsSync(sap)) {
      outcome = idempotentOutcome();
    } else if (!existsSync(sip)) {
      // Should be unreachable: discovery added this entry because it
      // existed in inbox OR archive; if neither exists now, skip it.
      continue;
    } else if (existsSync(sap)) {
      // Byte-identical already validated upstream.
      rmSync(sip);
      outcome = idempotentOutcome();
    } else {
      renameSync(sip, sap);
      outcome = { kind: 'moved', message: 'archived' };
    }
    attachmentOutcomes.push({ filename: s, outcome });
  }
  return { outcome: canonical, recipient, attachments: attachmentOutcomes };
}

function idempotentOutcome(): ArchiveOutcome {
  return {
    kind: 'idempotent',
    message: 'archived (idempotent: archive copy already present)',
  };
}

/**
 * Resolve a positional shape that may be `[]`, `[<filename>]`, or
 * `[<identity>, <filename>]`.
 *
 * Single-positional disambiguation: the `.md` suffix wins (identity
 * names can't contain `.` per LAYOUT-004). Pre-brief-017a this used
 * the strict `validFilename` grammar, which mis-parsed
 * `st message archive nope.md` as an identity and bailed with
 * the misleading "<filename> required" message.
 */
export function splitArchivePositionals(
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

// ─── archive trim ───────────────────────────────────────────────────────

export interface ArchiveTrimInput {
  recipient?: string | undefined;
  /** A duration spec like `30d`, `12h`, `2w`. */
  olderThan?: string | undefined;
  /** Keep this many most-recent archive files; trim the rest. */
  keepLast?: number | undefined;
  /** When true, list victims but delete nothing. */
  dryRun?: boolean;
  /**
   * Issue #8: when true, also delete prefix-sibling attachments of each
   * trimmed `.md` — symmetric with `st message archive
   * --with-attachments`. Default false matches the LAYOUT-004 "st
   * owns only the .md" semantic.
   */
  withAttachments?: boolean;

  env: NodeJS.ProcessEnv;
  stRoot: string;
  /** Override now() for testability. Defaults to {@link msNow}. */
  now?: () => number;
}

export interface ArchiveTrimResult {
  /** Filenames deleted (or that *would* be deleted under --dry-run). */
  victims: string[];
  /** Trailing summary line printed to stderr. */
  summary: string;
  /** Whether this was a dry run. */
  dryRun: boolean;
  /**
   * When {@link ArchiveTrimInput.withAttachments} was true, the
   * sibling files deleted (or that *would* be deleted under --dry-run)
   * alongside the canonical `.md` victims. Empty when no siblings
   * existed in archive. Undefined when withAttachments was false.
   */
  attachments?: string[];
}

export function cmdArchiveTrim(input: ArchiveTrimInput): ArchiveTrimResult {
  const recipient = resolveIdentity({
    explicit: input.recipient,
    env: input.env,
    stRoot: input.stRoot,
  });

  if (input.olderThan === undefined && input.keepLast === undefined) {
    throw new Error('trim requires --older-than DURATION or --keep-last N');
  }

  if (input.keepLast !== undefined) {
    if (
      !Number.isInteger(input.keepLast) ||
      (input.keepLast as number) < 0
    ) {
      throw new Error('--keep-last must be a non-negative integer');
    }
  }

  const adir = archiveDir(recipient, input.stRoot);
  const dryRun = input.dryRun === true;

  let entries: string[];
  try {
    entries = readdirSync(adir);
  } catch {
    return {
      victims: [],
      summary: dryRun
        ? '# would trim 0 files (dry run; nothing deleted)'
        : '# trimmed 0 files',
      dryRun,
    };
  }
  // Filter to grammar-valid filenames and sort chronologically.
  const files = entries.filter(validFilename).sort();

  const victims = new Set<string>();

  if (input.olderThan !== undefined) {
    const secs = parseDuration(input.olderThan);
    const now = input.now ? input.now() : msNow();
    const cutoffMs = now - secs * 1000;
    for (const f of files) {
      if (filenameTimestamp(f) < cutoffMs) victims.add(f);
    }
  }

  if (input.keepLast !== undefined) {
    const keep = input.keepLast;
    if (files.length > keep) {
      const drop = files.length - keep;
      for (let i = 0; i < drop; i++) {
        victims.add(files[i]!);
      }
    }
  }

  // Stable order: chronological (= filename ascending).
  const uniq = files.filter((f) => victims.has(f));

  // Issue #8: when --with-attachments, also collect prefix-siblings of
  // each victim from the same archive folder. Scanned from the full
  // directory listing rather than the grammar-filtered `files` since
  // siblings deliberately don't match the .md grammar.
  let attachments: string[] | undefined;
  if (input.withAttachments === true) {
    const victimPrefixes = new Set<string>();
    for (const v of uniq) victimPrefixes.add(v.slice(0, 20));
    const siblingSet = new Set<string>();
    for (const name of entries) {
      if (validFilename(name)) continue; // .md handled by the main loop
      const pre = prefixOf(name);
      if (pre === null) continue;
      if (victimPrefixes.has(pre)) siblingSet.add(name);
    }
    attachments = [...siblingSet].sort();
  }

  if (!dryRun) {
    for (const f of uniq) {
      try {
        rmSync(join(adir, f));
      } catch {
        // ignore: best-effort
      }
    }
    if (attachments !== undefined) {
      for (const f of attachments) {
        try {
          rmSync(join(adir, f));
        } catch {
          // ignore: best-effort
        }
      }
    }
  }

  const word = pluralize(uniq.length, 'file', 'files');
  const summary = dryRun
    ? `# would trim ${uniq.length} ${word} (dry run; nothing deleted)`
    : `# trimmed ${uniq.length} ${word}`;

  const result: ArchiveTrimResult = { victims: uniq, summary, dryRun };
  if (attachments !== undefined) result.attachments = attachments;
  return result;
}

const DURATION_RE = /^([0-9]+)([smhdw])$/;
const DURATION_UNITS: Record<string, number> = {
  s: 1,
  m: 60,
  h: 3600,
  d: 86400,
  w: 86400 * 7,
};

export { cmdArchive as cmdArchiveCore, cmdArchiveTrim as cmdArchiveTrimCore };

// ─── CLI wrapper ────────────────────────────────────────────────────────

import { invokedName, type CliContext } from '../cli-context.ts';

export function cmdArchiveCli(
  args: readonly string[],
  ctx: CliContext
): number {
  if (args[0] === 'trim') {
    return cmdArchiveTrimCli(args.slice(1), ctx);
  }
  let withAttachments = false;
  const positional: string[] = [];
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '--with-attachments':
        withAttachments = true;
        break;
      case '-h':
      case '--help':
        {
          const name = invokedName(ctx.env);
          ctx.stderr(
            `usage: ${name} message archive [<identity>] <filename> [--with-attachments]\n` +
              `       ${name} message archive trim [<identity>] [--older-than DURATION] [--keep-last N] [--dry-run] [--with-attachments]\n`
          );
        }
        return 0;
      default:
        if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
        positional.push(a);
    }
  }
  const { recipient, filename } = splitArchivePositionals(positional);
  if (filename === undefined) throw new Error('<filename> required');
  const r = cmdArchive({
    ...(recipient !== undefined && { recipient }),
    filename,
    withAttachments,
    env: ctx.env,
    stRoot: ctx.stRoot,
  });
  ctx.stderr(`${r.outcome.message}\n`);
  if (r.attachments !== undefined && r.attachments.length > 0) {
    const word = pluralize(r.attachments.length, 'attachment', 'attachments');
    ctx.stderr(`# also archived ${r.attachments.length} ${word}\n`);
    for (const att of r.attachments) {
      ctx.stderr(`  ${att.filename}: ${att.outcome.message}\n`);
    }
  }
  return 0;
}

function cmdArchiveTrimCli(args: readonly string[], ctx: CliContext): number {
  let recipient: string | undefined;
  let olderThan: string | undefined;
  let keepLast: number | undefined;
  let dryRun = false;
  let withAttachments = false;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '--older-than':
        olderThan = args[++i];
        break;
      case '--keep-last': {
        const v = args[++i];
        if (v === undefined || !/^[0-9]+$/.test(v)) {
          throw new Error('--keep-last must be a non-negative integer');
        }
        keepLast = Number(v);
        break;
      }
      case '--dry-run':
        dryRun = true;
        break;
      case '--with-attachments':
        withAttachments = true;
        break;
      case '-h':
      case '--help':
        ctx.stderr(
          `usage: ${invokedName(ctx.env)} message archive trim [<identity>] [--older-than DURATION] [--keep-last N] [--dry-run] [--with-attachments]\n`
        );
        return 0;
      default:
        if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
        if (recipient === undefined) recipient = a;
        else throw new Error(`unexpected arg: ${a}`);
    }
  }
  const r = cmdArchiveTrim({
    ...(recipient !== undefined && { recipient }),
    ...(olderThan !== undefined && { olderThan }),
    ...(keepLast !== undefined && { keepLast }),
    dryRun,
    withAttachments,
    env: ctx.env,
    stRoot: ctx.stRoot,
  });
  for (const f of r.victims) ctx.stdout(`${f}\n`);
  if (r.attachments !== undefined) {
    for (const a of r.attachments) ctx.stdout(`${a}\n`);
  }
  ctx.stderr(`${r.summary}\n`);
  if (r.attachments !== undefined && r.attachments.length > 0) {
    const word = pluralize(r.attachments.length, 'attachment', 'attachments');
    const verb = dryRun ? 'would also trim' : 'also trimmed';
    ctx.stderr(`# ${verb} ${r.attachments.length} ${word}\n`);
  }
  return 0;
}

/** Parse a duration like `30d` / `12h` / `2w` to seconds. */
export function parseDuration(spec: string): number {
  const m = DURATION_RE.exec(spec);
  if (!m) {
    throw new InvalidDurationError(spec);
  }
  const n = Number(m[1]);
  const unit = m[2]!;
  return n * DURATION_UNITS[unit]!;
}
