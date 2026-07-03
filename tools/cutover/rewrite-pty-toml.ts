// tools/cutover/rewrite-pty-toml.ts — pure rewriter for the
// coord→st `pty.toml` cutover.
//
// pty.toml is TOML, but the cutover only touches a small, stable set
// of shapes — env-var key names inside `[sessions.<name>.env]` tables
// and command tokens inside `command = "..."` lines. A line-based
// regex sweep is safer than a full TOML round-trip: it preserves the
// operator's formatting (blank lines, comments, key ordering) and
// only rewrites what actually needs changing.
//
// Handled shapes (all idempotent):
//
//   1. `command = "... server:coord ..."`      →  `... server:st ...`
//   2. `command = "... coord ding <arg>"`      →  `... st ding <arg>`
//      (word-boundary match so `coord dingo` etc. is safe)
//   3. `COORD_IDENTITY = "..."` inside a `.env` block → `ST_AGENT = "..."`
//      If the same block already sets `ST_AGENT`, the `COORD_IDENTITY`
//      line is dropped (redundant belt-and-suspenders). Same rule for
//      the legacy `ST_IDENTITY` line.
//   4. `COORD_ROOT = "..."`   → `ST_ROOT = "..."` (same conflict rule).
//   5. `COORD_CONFIG = "..."` → `ST_CONFIG = "..."` (same conflict rule).
//
// The rewriter walks the file once, tracking which `.env` table it's
// inside so it can enforce the "canonical wins over legacy" rule
// per-block instead of file-wide.

export interface RewriteResult {
  text: string;
  changed: boolean;
  actions: readonly string[];
}

const SECTION_RE = /^\[([^\]]+)\]\s*$/;
const ENV_SECTION_RE = /^\[sessions\.[^.\]]+\.env\]\s*$/;

/** Rewrite the `pty.toml` text per the cutover rules. */
export function rewritePtyToml(text: string): RewriteResult {
  const lines = text.split('\n');
  const actions: string[] = [];
  const out: string[] = [];

  // First pass — index each `[sessions.<name>.env]` block's line
  // range and record which canonical keys (`ST_AGENT`, `ST_ROOT`,
  // `ST_CONFIG`) it already sets. The second pass uses this map to
  // decide whether a legacy line should be renamed or dropped.
  const envBlockCanonicals = indexEnvBlockCanonicals(lines);

  let currentEnvBlock: number | null = null;
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]!;
    const section = SECTION_RE.exec(line);
    if (section !== null) {
      currentEnvBlock = ENV_SECTION_RE.test(line) ? i : null;
      out.push(line);
      continue;
    }

    // Command-line rewrites — apply outside env blocks too, since
    // top-level `command = "..."` isn't unheard of.
    const cmdRewritten = rewriteCommandLine(line, actions);
    if (cmdRewritten !== null) {
      out.push(cmdRewritten);
      continue;
    }

    // Env-line rewrites — only inside `[sessions.<name>.env]` tables.
    if (currentEnvBlock !== null) {
      const canonicalsInBlock = envBlockCanonicals.get(currentEnvBlock) ?? new Set<string>();
      const rewritten = rewriteEnvLine(line, canonicalsInBlock, actions);
      if (rewritten === null) {
        // dropped
        continue;
      }
      out.push(rewritten);
      continue;
    }

    out.push(line);
  }

  const changed = actions.length > 0;
  if (!changed) return { text, changed: false, actions };
  return { text: out.join('\n'), changed, actions };
}

/** First-pass scan. Returns a map from each `[sessions.<name>.env]`
 *  section's line index to the set of canonical keys it already
 *  sets. */
function indexEnvBlockCanonicals(
  lines: readonly string[]
): Map<number, Set<string>> {
  const map = new Map<number, Set<string>>();
  let current: number | null = null;
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]!;
    if (SECTION_RE.test(line)) {
      current = ENV_SECTION_RE.test(line) ? i : null;
      if (current !== null) map.set(current, new Set());
      continue;
    }
    if (current === null) continue;
    const kv = /^\s*([A-Z_][A-Z0-9_]*)\s*=/.exec(line);
    if (kv === null) continue;
    const key = kv[1]!;
    if (
      key === 'ST_AGENT' ||
      key === 'ST_ROOT' ||
      key === 'ST_CONFIG'
    ) {
      map.get(current)!.add(key);
    }
  }
  return map;
}

/** Rewrite one command-line entry. Returns the (possibly) modified
 *  line, or `null` when the line isn't a command line. */
function rewriteCommandLine(
  line: string,
  actions: string[]
): string | null {
  const cmdRe = /^(\s*command\s*=\s*)("(?:[^"\\]|\\.)*")\s*$/;
  const m = cmdRe.exec(line);
  if (m === null) return null;
  const prefix = m[1]!;
  const quoted = m[2]!;
  const before = quoted;

  // server:coord → server:st. Negative-lookahead guard rejects
  // any following identifier-shaped char (letter / digit /
  // underscore / hyphen) so e.g. `server:coord-web` or
  // `server:coordinator` (contrived, but a cheap safety) don't
  // false-match. `\b` alone would fail here — `-` is a non-word
  // char, so `\b` matches right before it.
  let after = quoted.replace(/server:coord(?![-a-zA-Z0-9_])/g, 'server:st');

  // `coord ding <arg>` → `st ding <arg>`. Word-boundary before
  // `coord` so `pty-coord` (unrelated repo path) doesn't match; the
  // trailing ` ding` anchors to the actual ding sidecar shape.
  after = after.replace(/\bcoord ding\b/g, 'st ding');

  if (after === before) return line;
  actions.push(`command: ${before} → ${after}`);
  return prefix + after;
}

/** Rewrite one env-line entry. Returns the (possibly) modified line,
 *  or `null` when the line should be dropped entirely (redundant
 *  legacy key). */
function rewriteEnvLine(
  line: string,
  canonicalsAlreadySet: Set<string>,
  actions: string[]
): string | null {
  const kvRe = /^(\s*)([A-Z_][A-Z0-9_]*)(\s*=\s*)(.*)$/;
  const m = kvRe.exec(line);
  if (m === null) return line;
  const indent = m[1]!;
  const key = m[2]!;
  const eq = m[3]!;
  const rest = m[4]!;

  const renames: Record<string, { to: string; drop: boolean }> = {
    COORD_IDENTITY: { to: 'ST_AGENT', drop: canonicalsAlreadySet.has('ST_AGENT') },
    ST_IDENTITY: { to: 'ST_AGENT', drop: canonicalsAlreadySet.has('ST_AGENT') },
    COORD_ROOT: { to: 'ST_ROOT', drop: canonicalsAlreadySet.has('ST_ROOT') },
    COORD_CONFIG: { to: 'ST_CONFIG', drop: canonicalsAlreadySet.has('ST_CONFIG') },
  };
  const spec = renames[key];
  if (spec === undefined) return line;

  if (spec.drop) {
    actions.push(`env: dropped redundant ${key} (${spec.to} already set)`);
    return null;
  }
  // Rename in place. Also update the "seen canonicals" tracker so a
  // later legacy sibling in the same block gets the drop path.
  canonicalsAlreadySet.add(spec.to);
  actions.push(`env: renamed ${key} → ${spec.to}`);
  return indent + spec.to + eq + rest;
}
