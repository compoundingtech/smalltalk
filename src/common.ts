// common.ts — shared helpers for the coord CLI (Node port).
//
// Mirror of lib/common.sh. Behavior is 1:1 with the bash reference where
// reasonable; minor improvements (newline-safe YAML quoting on emit/parse
// roundtrip) are noted in line.

import { randomBytes } from 'node:crypto';
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

import {
  IdentityNotHostedError,
  IdentityRequiredError,
  InvalidFilenameError,
  InvalidIdentityError,
} from './errors.ts';

// ─── Types ───────────────────────────────────────────────────────────────

export type Identity = string;
export type Filename = string;
// `unknown` is a derived state: the status file's mtime is older than
// STATUS_STALE_MS so we can't trust whatever's recorded there. Users
// cannot `coord status --set unknown` — see SETTABLE_STATES below and
// the validator in commands/status.ts.
// `away` (brief-029) is a fifth settable state meaning "present but not
// actively engaged" — distinct from `busy` (focused work, don't
// interrupt) and `offline` (gone). coord-web's
// document.visibilitychange handler is the canonical writer.
export type State =
  | 'offline'
  | 'available'
  | 'busy'
  | 'away'
  | 'dnd'
  | 'unknown';

export interface Message {
  fm: Record<string, unknown>;
  body: string;
}

// ─── Constants ───────────────────────────────────────────────────────────

export const CROCKFORD_BASE32 = '0123456789abcdefghjkmnpqrstvwxyz';

export const STATES: readonly State[] = [
  'offline',
  'available',
  'busy',
  'away',
  'dnd',
  'unknown',
];

/** States a user can write to disk via `coord status --set <state>`.
 *  `unknown` is derived from mtime staleness — the user can never set it
 *  directly. */
export const SETTABLE_STATES: readonly State[] = [
  'offline',
  'available',
  'busy',
  'away',
  'dnd',
];

/** A status file is considered stale (and read as `unknown` regardless
 *  of its on-disk value) when its mtime is older than this. 15 minutes
 *  is arbitrary but defensible: an actively-working agent re-writes its
 *  status implicitly via the boot ritual on each cold start, and the
 *  MCP server's periodic refresh (STATUS_REFRESH_MS below) keeps the
 *  mtime fresh in between. */
export const STATUS_STALE_MS = 15 * 60 * 1000;

/** How often a running MCP server re-writes its identity's status file
 *  to bump mtime. Preserves the recorded value; just proves liveness so
 *  peers don't see a healthy-but-idle agent fall into the `unknown`
 *  staleness window. 5 minutes gives a 3x safety ratio against
 *  STATUS_STALE_MS — two missed refreshes before `unknown` kicks in. */
export const STATUS_REFRESH_MS = 5 * 60 * 1000;

/** brief-030: how often the MCP server runs its tidy-check tick (the
 *  drift detector that nudges an agent when their inbox / tasks /
 *  journal are out of date relative to the boot ritual). Default 20
 *  minutes — long enough that an active agent isn't constantly
 *  reminded, short enough that drift gets caught within a working
 *  session. */
export const TIDY_CHECK_INTERVAL_MS = 20 * 60 * 1000;

/** Drift threshold: an inbox file older than this is "unaddressed."
 *  Tunable per myobie's loose initial framing. */
export const STALE_INBOX_MS = 10 * 60 * 1000;

/** Drift threshold: a task in `doing` whose file hasn't been touched
 *  in this long has probably been abandoned without a status flip. */
export const STALE_DOING_TASK_MS = 60 * 60 * 1000;

/** Drift threshold: when the latest journal entry is older than this
 *  AND a task has transitioned to `done` since that entry was
 *  written, the agent has shipped without journaling — drift. */
export const STALE_JOURNAL_MS = 60 * 60 * 1000;

export const RESERVED_NAMES: readonly string[] = [
  // Per-identity sub-folders / sidecars (LAYOUT-004 + brief-015 +
  // brief-024). Each is a folder or file inside an identity dir; an
  // identity name that collides with one of these would shadow it.
  'inbox',
  'archive',
  'tasks',
  'journal',
  'status',
  'name',
  // Status states (would alias-collide with `coord status <token>`).
  // `unknown` is the brief-022 derived staleness value; reserved so
  // it can't double as an identity name either. `away` (brief-029) is
  // the fifth settable state.
  'offline',
  'available',
  'busy',
  'away',
  'dnd',
  'unknown',
  // Verb names that the CLI enumerates the root for (brief-016).
  // Reserving them keeps the verb-vs-identity ambiguity out.
  'members',
  'overview',
];

const FILENAME_RE = /^[0-9]{13}-[0-9a-z]{6}\.md$/;
// Identities are lowercase ASCII alphanumeric + hyphens and PERIODS,
// starting and ending with an alphanumeric. The period (issue #1) is
// a convention for encoding hierarchy in a flat namespace, e.g.
// `orchestrator.session-1.child-7`. Real nested folders (paths with
// `/`) are deliberately NOT supported — see issue #1 for the
// decision and the use-cases that prompted the question.
const IDENTITY_RE = /^[a-z0-9]([a-z0-9.-]*[a-z0-9])?$/;

// ─── Pluralize ───────────────────────────────────────────────────────────

export function pluralize(n: number, singular: string, plural: string): string {
  return n === 1 ? singular : plural;
}

// ─── Time ────────────────────────────────────────────────────────────────

export function msNow(): number {
  return Date.now();
}

export function rfc3339FromMs(ms: number): string {
  return new Date(ms).toISOString();
}

// ─── Filename helpers ────────────────────────────────────────────────────

export function rand6(): string {
  const bytes = randomBytes(6);
  let out = '';
  for (let i = 0; i < 6; i++) {
    out += CROCKFORD_BASE32[bytes[i]! % 32];
  }
  return out;
}

export function genFilename(): Filename {
  return `${msNow()}-${rand6()}.md`;
}

export function validFilename(s: string): boolean {
  return FILENAME_RE.test(s);
}

export function filenameTimestamp(s: string): number {
  if (!validFilename(s)) {
    throw new InvalidFilenameError(s);
  }
  return Number(s.slice(0, 13));
}

// ─── Identity ────────────────────────────────────────────────────────────

export function validIdentity(s: string): boolean {
  if (!s) return false;
  if (!IDENTITY_RE.test(s)) return false;
  if (RESERVED_NAMES.includes(s)) return false;
  return true;
}

export interface ResolveIdentityOpts {
  /** explicit identity from `--from`, positional, etc. */
  explicit?: string | undefined;
  /** environment to read COORD_IDENTITY from (defaults to process.env). */
  env?: NodeJS.ProcessEnv;
  /** override $COORD_ROOT used for the folder-existence check. */
  coordRoot?: string;
  /**
   * Folder-existence policy when {@link explicit} is set.
   * - `undefined` (default) → both `<id>/inbox` AND `<id>/archive`
   *   must exist. Use for writes (`--from <other>`) so the anti-
   *   impersonation invariant holds: you can't act AS another
   *   identity by half-fabricating their folder.
   * - `'lenient'` → at least ONE of inbox/archive must exist. Use
   *   for cross-identity reads (`coord ls <other>`, `coord read
   *   <other>`, `coord thread <other>`, `coord watch <other>`):
   *   a peer's folder is often partial on this machine — a single
   *   `coord send <other>` lazily creates inbox/ but not archive/.
   *   The actual file lookups below this gate are existsSync-gated,
   *   so a missing folder is naturally treated as empty.
   *
   * Ignored when `explicit` is unset — $COORD_IDENTITY always
   * auto-creates regardless of `policy`.
   */
  policy?: 'lenient';
}

/**
 * Resolve the active identity per LAYOUT-004 "Identity resolution":
 *   1. explicit arg if non-empty
 *   2. $COORD_IDENTITY env var
 *   3. throw "identity required"
 *
 * Folder-existence handling differs by source:
 *   - $COORD_IDENTITY (no explicit) → lazy bootstrap. First command
 *     for a new identity creates `<id>/{inbox,archive}` on demand.
 *     This is "you, claiming to be you" — no risk of impersonation.
 *   - explicit (--from <other>, positional <recipient>, etc.) →
 *     folder must already exist. Throws IdentityNotHostedError if
 *     not. Preserves anti-impersonation: you can't claim to act AS
 *     another identity by fabricating their folder on this machine.
 */
export function resolveIdentity(opts: ResolveIdentityOpts = {}): string {
  const env = opts.env ?? process.env;
  const root = opts.coordRoot ?? coordRootFrom(env);

  let id: string;
  let fromExplicit: boolean;
  if (opts.explicit) {
    id = opts.explicit;
    fromExplicit = true;
  } else if (env.COORD_IDENTITY) {
    id = env.COORD_IDENTITY;
    fromExplicit = false;
  } else {
    throw new IdentityRequiredError();
  }
  if (!validIdentity(id)) {
    throw new InvalidIdentityError(id);
  }
  if (fromExplicit) {
    if (opts.policy === 'lenient') {
      assertIdentityFolderExistsLenient(id, root);
    } else {
      assertIdentityFolderExists(id, root);
    }
  } else {
    // Implicit ($COORD_IDENTITY) always auto-creates — "you,
    // claiming to be you" needs no anti-impersonation guard, and
    // a brand-new identity should bootstrap on its first command.
    ensureIdentityDirs(id, root);
  }
  return id;
}

// ─── Paths ───────────────────────────────────────────────────────────────

export function coordRootFrom(env: NodeJS.ProcessEnv = process.env): string {
  const v = env.COORD_ROOT;
  return v && v.length > 0 ? v : join(homedir(), '.local/state/coord');
}

export function coordConfigFrom(env: NodeJS.ProcessEnv = process.env): string {
  const v = env.COORD_CONFIG;
  return v && v.length > 0 ? v : join(homedir(), '.config/coord');
}

export function coordRoot(): string {
  return coordRootFrom();
}

export function coordConfig(): string {
  return coordConfigFrom();
}

export function identityDir(id: string, root: string = coordRoot()): string {
  return join(root, id);
}

export function inboxDir(id: string, root: string = coordRoot()): string {
  return join(root, id, 'inbox');
}

export function archiveDir(id: string, root: string = coordRoot()): string {
  return join(root, id, 'archive');
}

export function tasksDir(id: string, root: string = coordRoot()): string {
  return join(root, id, 'tasks');
}

export function journalDir(id: string, root: string = coordRoot()): string {
  return join(root, id, 'journal');
}

export function statusPath(id: string, root: string = coordRoot()): string {
  return join(root, id, 'status');
}

export function namePath(id: string, root: string = coordRoot()): string {
  return join(root, id, 'name');
}

// ─── Folder existence ────────────────────────────────────────────────────

export function assertIdentityFolderExists(
  id: string,
  root: string = coordRoot()
): void {
  const inbox = inboxDir(id, root);
  const archive = archiveDir(id, root);
  if (!isDir(inbox) || !isDir(archive)) {
    throw new IdentityNotHostedError(id);
  }
}

/**
 * Lenient form of {@link assertIdentityFolderExists}: succeeds if at
 * least one of `<id>/inbox` or `<id>/archive` exists. Used by
 * cross-identity read paths (ls, read, thread, watch), where peer
 * folders are often partial — e.g. `coord send bob` lazily creates
 * `bob/inbox/` but not `bob/archive/`, so a follow-up
 * `coord ls bob` from the sender's side would fail under the strict
 * check even though there's a message to read.
 */
export function assertIdentityFolderExistsLenient(
  id: string,
  root: string = coordRoot()
): void {
  const inbox = inboxDir(id, root);
  const archive = archiveDir(id, root);
  if (!isDir(inbox) && !isDir(archive)) {
    throw new IdentityNotHostedError(id);
  }
}

function isDir(p: string): boolean {
  try {
    return statSync(p).isDirectory();
  } catch {
    return false;
  }
}

export function ensureIdentityDirs(
  id: string,
  root: string = coordRoot()
): void {
  mkdirSync(inboxDir(id, root), { recursive: true });
  mkdirSync(archiveDir(id, root), { recursive: true });
}

// ─── Frontmatter ─────────────────────────────────────────────────────────

/**
 * Parse a markdown file with optional YAML frontmatter.
 *
 * Permissive on read (matches the bash impl):
 * - No `---` opening fence on line 1 → no frontmatter, body = whole text.
 * - Opening fence but no closing fence → no frontmatter, body = whole text.
 * - Empty input → `{ fm: {}, body: "" }`.
 * - Inside the fence: comment lines (`#...`), blank lines, and lines that
 *   don't match `key: value` are silently ignored.
 *
 * Top-level scalars only — `tags: [a, b]` ends up as the literal string
 * `"[a, b]"` (per the brief's "permissive shape" rule).
 */
export function parseFrontmatter(text: string): Message {
  const lines = text.split('\n');
  if (lines.length === 0 || lines[0] !== '---') {
    return { fm: {}, body: text };
  }
  let closeIdx = -1;
  for (let i = 1; i < lines.length; i++) {
    if (lines[i] === '---') {
      closeIdx = i;
      break;
    }
  }
  if (closeIdx === -1) {
    // Permissive: unterminated fence is treated as body.
    return { fm: {}, body: text };
  }
  const fm: Record<string, unknown> = {};
  for (let i = 1; i < closeIdx; i++) {
    const line = lines[i] ?? '';
    if (/^[ \t]*$/.test(line)) continue;
    if (/^[ \t]*#/.test(line)) continue;
    const m = /^[ \t]*([A-Za-z0-9_-]+)[ \t]*:[ \t]*(.*)$/.exec(line);
    if (!m) continue;
    const key = m[1]!;
    let raw = m[2]!.replace(/[ \t]+$/, '');
    fm[key] = unwrapYamlScalar(raw);
  }
  const body = lines.slice(closeIdx + 1).join('\n');
  return { fm, body };
}

/** Strip surrounding quotes (if present) and unescape double-quote contents. */
function unwrapYamlScalar(raw: string): string {
  if (raw.length >= 2) {
    const first = raw[0];
    const last = raw[raw.length - 1];
    if (first === '"' && last === '"') {
      return unescapeYamlDouble(raw.slice(1, -1));
    }
    if (first === "'" && last === "'") {
      return raw.slice(1, -1);
    }
  }
  return raw;
}

function unescapeYamlDouble(s: string): string {
  let out = '';
  for (let i = 0; i < s.length; i++) {
    const c = s[i]!;
    if (c === '\\' && i + 1 < s.length) {
      const n = s[i + 1]!;
      switch (n) {
        case 'n':
          out += '\n';
          break;
        case 'r':
          out += '\r';
          break;
        case 't':
          out += '\t';
          break;
        case '\\':
          out += '\\';
          break;
        case '"':
          out += '"';
          break;
        default:
          out += n;
          break;
      }
      i++;
    } else {
      out += c;
    }
  }
  return out;
}

/**
 * Emit a markdown file with YAML frontmatter from {@link Message}.
 *
 * Identifier-shape string values (matching `[A-Za-z0-9_-]+`) emit as
 * plain scalars (`from: alice`); all other strings are double-quoted via
 * {@link yamlQuote}. Arrays emit as inline lists with each element quoted
 * (`tags: ["a", "b"]`). Numbers and booleans emit as plain scalars.
 *
 * Output shape: `---\n<keys>\n---\n<body>` (no trailing newline added by
 * this function — call sites append one if the body doesn't already end
 * in `\n`).
 */
export function emitFrontmatter(
  fm: Record<string, unknown>,
  body: string
): string {
  let out = '---\n';
  for (const [k, v] of Object.entries(fm)) {
    if (v === undefined) continue;
    out += `${k}: ${formatYamlValue(v)}\n`;
  }
  out += '---\n';
  out += body;
  return out;
}

function formatYamlValue(v: unknown): string {
  if (typeof v === 'string') {
    // Plain-scalar safe identifier shape: alphanumerics + _ - .
    // The `.` lets filename values like `1714826789012-x9k4mz.md` emit
    // unquoted (matches the bash `printf 'in-reply-to: %s\n'` shape).
    if (/^[A-Za-z0-9_.-]+$/.test(v)) return v;
    return yamlQuote(v);
  }
  if (Array.isArray(v)) {
    // Plain-scalar elements stay unquoted (so `tags: [auth, backend]`
    // round-trips cleanly through parseTagsScalar); anything that
    // needs quoting falls back to the double-quoted form.
    const parts = v.map((x) => {
      const s = String(x);
      return /^[A-Za-z0-9_.-]+$/.test(s) ? s : yamlQuote(s);
    });
    return `[${parts.join(', ')}]`;
  }
  if (typeof v === 'number' || typeof v === 'boolean') {
    return String(v);
  }
  return yamlQuote(String(v));
}

/**
 * Emit a YAML-safe double-quoted scalar. Always wraps in `"..."`. Escapes
 * `\\`, `"`, `\n`, `\r`, `\t` so a free-form value (with colons, hashes,
 * leading whitespace, embedded newlines, etc.) survives a roundtrip.
 */
export function yamlQuote(s: string): string {
  const escaped = s
    .replace(/\\/g, '\\\\')
    .replace(/"/g, '\\"')
    .replace(/\n/g, '\\n')
    .replace(/\r/g, '\\r')
    .replace(/\t/g, '\\t');
  return `"${escaped}"`;
}

// ─── Atomic write ────────────────────────────────────────────────────────

/**
 * Write `content` to `filePath` with `O_EXCL` semantics (fail if the file
 * already exists). Throws on any failure — including a missing parent
 * directory.
 */
export function safeAtomicWrite(
  filePath: string,
  content: string | Buffer
): void {
  writeFileSync(filePath, content, { flag: 'wx' });
}

// ─── Sweep ───────────────────────────────────────────────────────────────

export interface SweepResult {
  removed: number;
}

/**
 * Enforce the LAYOUT archive-as-tombstone invariant: for every
 * `<id>/archive/X.md` under `coordRoot`, remove `<id>/inbox/X.md` if it
 * is byte-identical to the archive copy. Skips divergent pairs (a violated
 * invariant — likely manual edit; surfaced by archive case-3). Idempotent.
 */
export function sweep(rootArg?: string): SweepResult {
  const root = rootArg ?? coordRoot();
  let removed = 0;

  let topEntries: string[];
  try {
    topEntries = readdirSync(root);
  } catch {
    return { removed: 0 };
  }

  for (const id of topEntries) {
    const archDir = join(root, id, 'archive');
    let archEntries: string[];
    try {
      archEntries = readdirSync(archDir);
    } catch {
      continue;
    }
    for (const name of archEntries) {
      if (!validFilename(name)) continue;
      const inboxPath = join(root, id, 'inbox', name);
      const archivePath = join(archDir, name);
      if (!existsSync(inboxPath)) continue;
      let inboxBuf: Buffer;
      let archiveBuf: Buffer;
      try {
        inboxBuf = readFileSync(inboxPath);
        archiveBuf = readFileSync(archivePath);
      } catch {
        continue;
      }
      if (inboxBuf.equals(archiveBuf)) {
        try {
          rmSync(inboxPath);
          removed++;
        } catch {
          // ignore unlink failures — sweep is best-effort
        }
      }
    }
  }

  return { removed };
}
