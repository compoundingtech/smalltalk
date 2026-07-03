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
  AgentNotHostedError,
  AgentRequiredError,
  InvalidAgentError,
  InvalidFilenameError,
} from './errors.ts';

// ─── Types ───────────────────────────────────────────────────────────────

export type Agent = string;
/** @deprecated Use {@link Agent}. */
export type Identity = Agent;
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
 *  drift detector that nudges an agent when their inbox is out of date
 *  relative to the boot ritual). Default 20 minutes — long enough that
 *  an active agent isn't constantly reminded, short enough that drift
 *  gets caught within a working session. */
export const TIDY_CHECK_INTERVAL_MS = 20 * 60 * 1000;

/** Drift threshold: an inbox file older than this is "unaddressed."
 *  Tunable per myobie's loose initial framing. */
export const STALE_INBOX_MS = 10 * 60 * 1000;

export const RESERVED_NAMES: readonly string[] = [
  // Per-agent sub-folders / sidecars. Each is a folder or file inside
  // an agent dir; an agent name that collides with one of these would
  // shadow it.
  'inbox',
  'archive',
  'resources',
  'status',
  'name',
  // Status states (would alias-collide with `coord status <token>`).
  // `unknown` is the brief-022 derived staleness value; reserved so
  // it can't double as an agent name either. `away` (brief-029) is
  // the fifth settable state.
  'offline',
  'available',
  'busy',
  'away',
  'dnd',
  'unknown',
  // Verb names that the CLI enumerates the root for (brief-016).
  // Reserving them keeps the verb-vs-agent ambiguity out. `members`
  // is the deprecated alias of `agents`; reserved for both.
  'members',
  'agents',
  'overview',
];

const FILENAME_RE = /^[0-9]{13}-[0-9a-z]{6}\.md$/;
// Agent names are lowercase ASCII alphanumeric + hyphens and PERIODS,
// starting and ending with an alphanumeric. The period (issue #1) is
// a convention for encoding hierarchy in a flat namespace, e.g.
// `orchestrator.session-1.child-7`. Real nested folders (paths with
// `/`) are deliberately NOT supported — see issue #1 for the
// decision and the use-cases that prompted the question.
const AGENT_RE = /^[a-z0-9]([a-z0-9.-]*[a-z0-9])?$/;

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

/**
 * A `.md` file that landed in an inbox but does NOT match the
 * canonical LAYOUT-004 `<unix-ms>-<rand6>.md` grammar. Delivered as
 * an "outside message" instead of silently dropped so a collaborator
 * unfamiliar with the naming convention can still reach an agent.
 *
 * Accepted iff: ends in `.md`, is a plain basename (no path
 * separators / traversal / dotfile), and is not a prefix-sibling
 * attachment of a canonical `.md` (those already have a paired
 * canonical `.md` responsible for delivery).
 */
export function validOutsideFilename(s: string): boolean {
  if (!s.endsWith('.md')) return false;
  if (s.length <= 3) return false; // '.md' alone or shorter
  if (validFilename(s)) return false; // canonical LAYOUT-004
  if (s.includes('/') || s.includes('\\')) return false;
  if (s.startsWith('.')) return false; // hidden files / dot-prefixed
  if (prefixOf(s) !== null) return false; // attachment sidecar of a canonical .md
  return true;
}

/**
 * The union: filenames the channel-watcher / ls / read / archive
 * paths accept in an inbox. Canonical LAYOUT-004 OR outside `.md`.
 * Callers that need strict canonical semantics (timestamp derivation,
 * thread walking, sweep pairing) must keep using {@link validFilename}.
 */
export function validDeliverableFilename(s: string): boolean {
  return validFilename(s) || validOutsideFilename(s);
}

export function filenameTimestamp(s: string): number {
  if (!validFilename(s)) {
    throw new InvalidFilenameError(s);
  }
  return Number(s.slice(0, 13));
}

/**
 * The shared `<unix-ms>-<rand6>` prefix of a message file or attachment
 * sibling — 20 characters: `1719012345-abc123`. Used to associate
 * prefix-siblings with their canonical `.md` (LAYOUT-004 §Attachments).
 *
 * Returns the prefix iff `name` starts with the LAYOUT-004 grammar
 * followed by `.` and at least one suffix character. Returns `null` for
 * random files in inbox/archive (e.g. `.DS_Store`, `README`) so callers
 * can safely ignore non-message entries.
 *
 * Examples:
 *   prefixOf('1719012345-abc123.md')           → '1719012345-abc123'
 *   prefixOf('1719012345-abc123.options.json') → '1719012345-abc123'
 *   prefixOf('1719012345-abc123')              → null  (no extension)
 *   prefixOf('readme.md')                      → null
 */
const PREFIX_PART_RE = /^([0-9]{13}-[0-9a-z]{6})\..+$/;
export function prefixOf(name: string): string | null {
  const m = PREFIX_PART_RE.exec(name);
  return m ? m[1]! : null;
}

// ─── Agent ───────────────────────────────────────────────────────────────

export function validAgent(s: string): boolean {
  if (!s) return false;
  if (!AGENT_RE.test(s)) return false;
  if (RESERVED_NAMES.includes(s)) return false;
  return true;
}

/** @deprecated Use {@link validAgent}. */
export const validIdentity = validAgent;

export interface ResolveAgentOpts {
  /** explicit agent name from `--from`, positional, etc. */
  explicit?: string | undefined;
  /** environment to read ST_AGENT from (defaults to process.env). */
  env?: NodeJS.ProcessEnv;
  /** override $ST_ROOT used for the folder-existence check. */
  coordRoot?: string;
  /**
   * Folder-existence policy when {@link explicit} is set.
   * - `undefined` (default) → both `<agent>/inbox` AND `<agent>/archive`
   *   must exist. Use for writes (`--from <other>`) so the anti-
   *   impersonation invariant holds: you can't act AS another agent by
   *   half-fabricating their folder.
   * - `'lenient'` → at least ONE of inbox/archive must exist. Use for
   *   cross-agent reads (`coord ls <other>`, `coord read <other>`,
   *   `coord thread <other>`, `coord watch <other>`): a peer's folder
   *   is often partial on this machine — a single `coord send <other>`
   *   lazily creates inbox/ but not archive/. The actual file lookups
   *   below this gate are existsSync-gated, so a missing folder is
   *   naturally treated as empty.
   *
   * Ignored when `explicit` is unset — $ST_AGENT always auto-creates
   * regardless of `policy`.
   */
  policy?: 'lenient';
}

/** @deprecated Use {@link ResolveAgentOpts}. */
export type ResolveIdentityOpts = ResolveAgentOpts;

/**
 * Resolve the active agent per LAYOUT-004 "Agent resolution":
 *   1. explicit arg if non-empty
 *   2. $ST_AGENT env var (preferred)
 *   3. $ST_IDENTITY env var (deprecated, warns once per process)
 *   4. $COORD_IDENTITY env var (legacy, warns once per process)
 *   5. throw "agent required"
 *
 * Folder-existence handling differs by source:
 *   - env-resolved (no explicit) → lazy bootstrap. First command for
 *     a new agent creates `<agent>/{inbox,archive}` on demand. This is
 *     "you, claiming to be you" — no risk of impersonation.
 *   - explicit (--from <other>, positional <recipient>, etc.) → folder
 *     must already exist. Throws AgentNotHostedError if not. Preserves
 *     anti-impersonation: you can't claim to act AS another agent by
 *     fabricating their folder on this machine.
 *
 * brief-009 item 3 (rename): the env-var fallback is now three levels
 * deep — ST_AGENT → ST_IDENTITY (warn) → COORD_IDENTITY (warn). The
 * two legacy names buy cos time to sweep ~8 pty.toml files at her own
 * pace without breaking running agents.
 */
export function resolveAgent(opts: ResolveAgentOpts = {}): string {
  const env = opts.env ?? process.env;
  const root = opts.coordRoot ?? coordRootFrom(env);

  let id: string;
  let fromExplicit: boolean;
  if (opts.explicit) {
    id = opts.explicit;
    fromExplicit = true;
  } else if (env.ST_AGENT) {
    id = env.ST_AGENT;
    fromExplicit = false;
  } else if (env.ST_IDENTITY) {
    id = env.ST_IDENTITY;
    fromExplicit = false;
    warnCoordFallback('ST_AGENT', 'ST_IDENTITY');
  } else if (env.COORD_IDENTITY) {
    id = env.COORD_IDENTITY;
    fromExplicit = false;
    warnCoordFallback('ST_AGENT', 'COORD_IDENTITY');
  } else {
    throw new AgentRequiredError();
  }
  if (!validAgent(id)) {
    throw new InvalidAgentError(id);
  }
  if (fromExplicit) {
    if (opts.policy === 'lenient') {
      assertIdentityFolderExistsLenient(id, root);
    } else {
      assertIdentityFolderExists(id, root);
    }
  } else {
    // Implicit ($ST_AGENT or fallback) always auto-creates — "you,
    // claiming to be you" needs no anti-impersonation guard, and a
    // brand-new agent should bootstrap on its first command.
    ensureIdentityDirs(id, root);
  }
  return id;
}

/** @deprecated Use {@link resolveAgent}. */
export const resolveIdentity = resolveAgent;

// ─── Paths ───────────────────────────────────────────────────────────────
//
// brief-005-phase0 (issue: rename coord → smalltalk/st): the resolver
// honors BOTH the new `ST_*` names and the legacy `COORD_*` names; ST_
// wins when both are set. The fallback emits a one-time stderr notice
// per env-var-name per process so operators see which configs still
// reference the old name without breakage. The default state path
// prefers `~/.local/state/smalltalk` when it exists, falls back to
// `~/.local/state/coord` when only that exists, and creates
// `~/.local/state/smalltalk` on a brand-new install. Config dir
// (~/.config/coord) is intentionally NOT included in this phase per
// the brief — it ships in a later phase.

const warnedCoordFallbacks = new Set<string>();

/** Test-only: drop the one-time-warning latch so each test can observe
 *  the warning fresh. Not exported from the package barrel. */
export function _resetCoordFallbackWarnings(): void {
  warnedCoordFallbacks.clear();
}

function warnCoordFallback(stName: string, coordName: string): void {
  if (warnedCoordFallbacks.has(coordName)) return;
  warnedCoordFallbacks.add(coordName);
  // Best-effort — never crash a CLI invocation because the warning
  // pipe is closed (rare; pre-shutdown stderr can be EPIPE under
  // pipelines).
  try {
    process.stderr.write(
      `[smalltalk] honoring ${coordName} — migrate to ${stName} when convenient\n`
    );
  } catch {
    // ignore: we tried to warn, no point in killing the call over it
  }
}

function defaultStateRoot(env: NodeJS.ProcessEnv): string {
  const home = env.HOME ?? homedir();
  const stPath = join(home, '.local/state/smalltalk');
  const coordPath = join(home, '.local/state/coord');
  // Both exist (Phase 1 migration in flight on this machine) → prefer
  // the new path silently. cos clarified the warning belongs on env
  // vars (the actionable signal), not on the state-dir itself.
  try {
    if (statSync(stPath).isDirectory()) return stPath;
  } catch {
    // not present
  }
  try {
    if (statSync(coordPath).isDirectory()) return coordPath;
  } catch {
    // not present
  }
  return stPath; // brand-new install: the new path becomes the default
}

export function coordRootFrom(env: NodeJS.ProcessEnv = process.env): string {
  const stv = env.ST_ROOT;
  if (stv && stv.length > 0) return stv;
  const cv = env.COORD_ROOT;
  if (cv && cv.length > 0) {
    warnCoordFallback('ST_ROOT', 'COORD_ROOT');
    return cv;
  }
  return defaultStateRoot(env);
}

export function coordConfigFrom(env: NodeJS.ProcessEnv = process.env): string {
  const v = env.COORD_CONFIG;
  return v && v.length > 0 ? v : join(homedir(), '.config/coord');
}

/**
 * Read the agent name from `ST_AGENT` (preferred), `ST_IDENTITY`
 * (deprecated, one-time fallback notice), or `COORD_IDENTITY` (legacy,
 * one-time fallback notice). Returns `undefined` when none are set —
 * callers throw their own context-specific error. Mirrors the
 * env-fallback half of {@link resolveAgent} for code paths (e.g.
 * `coord mcp`) that read the env directly.
 */
export function envAgentFrom(
  env: NodeJS.ProcessEnv = process.env
): string | undefined {
  if (env.ST_AGENT && env.ST_AGENT.length > 0) return env.ST_AGENT;
  if (env.ST_IDENTITY && env.ST_IDENTITY.length > 0) {
    warnCoordFallback('ST_AGENT', 'ST_IDENTITY');
    return env.ST_IDENTITY;
  }
  if (env.COORD_IDENTITY && env.COORD_IDENTITY.length > 0) {
    warnCoordFallback('ST_AGENT', 'COORD_IDENTITY');
    return env.COORD_IDENTITY;
  }
  return undefined;
}

/** @deprecated Use {@link envAgentFrom}. */
export const envIdentityFrom = envAgentFrom;

/**
 * The name the CLI was invoked as (`coord` / `st` / `smalltalk`).
 * Set by the bash shim via `_ST_INVOKED_AS` BEFORE any symlink walk so
 * we capture the user-typed name, not the resolved real path. Defaults
 * to `coord` for back-compat with callers (tests, library embeds) that
 * never set the env var.
 */
export type InvokedAs = 'coord' | 'st' | 'smalltalk';

export function invokedAsFrom(
  env: NodeJS.ProcessEnv = process.env
): InvokedAs {
  const raw = env._ST_INVOKED_AS;
  if (raw === 'st' || raw === 'smalltalk' || raw === 'coord') return raw;
  return 'coord';
}

/** Canonicalize an invocation name down to `coord` (legacy alias) or
 *  `st` (the new short canonical name). `smalltalk` resolves to `st`. */
export function canonicalServerName(invoked: InvokedAs): 'coord' | 'st' {
  return invoked === 'coord' ? 'coord' : 'st';
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

export function resourcesDir(id: string, root: string = coordRoot()): string {
  return join(root, id, 'resources');
}

// brief-024 (context/ v1): per-agent durable working-state, the in-context-
// state leg of lossless-restart. Two surfaces:
//   now.md         — whole-file rewrite; last-write-wins snapshot of
//                    "what I'm mid-doing right now"
//   decisions/     — folder, one file per decision entry, named
//                    `<unix-ms>-<rand6>.md` (same LAYOUT-004 grammar as
//                    message inboxes). Append semantics = create-a-new-
//                    file; no rewriting the whole log to add a line, no
//                    clobbering on race, sorts by name = chronological
//                    for free. Nathan-approved shape (2026-07-03) —
//                    replaces the pre-review decisions.md single-file
//                    log after cos+Nathan flagged the append-in-place
//                    footguns.
// Both are absent-able: missing folder / missing files must not crash
// callers. Every helper reflects that — reads on absence return empty,
// writes lazy-create the folder. Rationale: the restart-continuity eval
// needs a control arm with no `context/` that fails exactly like an
// unrestored agent, plus a treatment arm that resumes cleanly.
export function contextDir(id: string, root: string = coordRoot()): string {
  return join(root, id, 'context');
}

export function contextNowPath(
  id: string,
  root: string = coordRoot()
): string {
  return join(contextDir(id, root), 'now.md');
}

export function contextDecisionsDir(
  id: string,
  root: string = coordRoot()
): string {
  return join(contextDir(id, root), 'decisions');
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
    throw new AgentNotHostedError(id);
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
    throw new AgentNotHostedError(id);
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
 *
 * Issue #8 extension: the rule also applies to prefix-sibling attachments
 * — `archive/X.options.json` byte-identical to `inbox/X.options.json`
 * is removed from inbox iff a matching `archive/X.md` exists, so the
 * file is recognized as part of a real coord message family rather than
 * an unrelated user-dropped file.
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
    // Coord-owned: a `.md` matching LAYOUT-004 grammar, OR an attachment
    // whose prefix matches one. This filter is what prevents sweep from
    // acting on random files a user manually dropped into archive/.
    const mdPrefixes = new Set<string>();
    for (const name of archEntries) {
      if (validFilename(name)) mdPrefixes.add(name.slice(0, 20));
    }
    for (const name of archEntries) {
      const isMd = validFilename(name);
      const pre = isMd ? null : prefixOf(name);
      const isFamily = isMd || (pre !== null && mdPrefixes.has(pre));
      if (!isFamily) continue;
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
