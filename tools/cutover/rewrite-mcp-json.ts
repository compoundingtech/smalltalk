// tools/cutover/rewrite-mcp-json.ts — pure rewriter for the
// coord→st `.mcp.json` cutover.
//
// Input is the raw JSON text of a `.mcp.json`. Output is the
// rewritten text (or the original, verbatim, when no cutover-relevant
// content is present). The rewriter handles four categories of change
// in one pass:
//
//   1. `mcpServers.coord` → `mcpServers.st` (rename the JSON key).
//   2. `command` path rewrite:
//        - `.../myobie/coord/bin/coord`     → `.../myobie/smalltalk/bin/st`
//        - `.../myobie/smalltalk/bin/coord` → `.../myobie/smalltalk/bin/st`
//        - `.../myobie/coord/bin/st`        → `.../myobie/smalltalk/bin/st`
//      Any other suffix (already `smalltalk/bin/st`) passes through.
//   3. `env.COORD_IDENTITY` → `env.ST_AGENT` (rename key, preserve value).
//      If both `ST_AGENT` and `COORD_IDENTITY` are present, drop the
//      `COORD_IDENTITY` (redundant belt-and-suspenders leftover).
//      Same treatment for `ST_IDENTITY` (drop when `ST_AGENT` set).
//   4. `env.COORD_ROOT` → `env.ST_ROOT` (same conflict rule).
//      `env.COORD_CONFIG` → `env.ST_CONFIG` (same).
//
// Idempotent: running the rewriter twice against the same file
// produces the same result the second call reports `changed: false`.

export interface RewriteResult {
  /** The (possibly) rewritten JSON text. Preserves trailing newline
   *  when the input had one. */
  text: string;
  /** True iff the rewriter actually mutated the JSON. Callers use this
   *  to decide whether to write to disk / print a diff. */
  changed: boolean;
  /** Human-readable summary of what changed, one line per action.
   *  Empty when `changed` is false. */
  actions: readonly string[];
}

/**
 * Rewrite the `.mcp.json` text per the coord→st cutover rules. Never
 * throws for well-formed JSON — an unrecognized shape passes through
 * unchanged. Throws only on parse failure so callers can decide to
 * skip the file.
 */
export function rewriteMcpJson(text: string): RewriteResult {
  const parsed = JSON.parse(text) as unknown;
  const actions: string[] = [];

  if (
    typeof parsed !== 'object' ||
    parsed === null ||
    Array.isArray(parsed)
  ) {
    return { text, changed: false, actions };
  }

  const obj = parsed as Record<string, unknown>;
  const mcp = obj.mcpServers;
  if (
    typeof mcp !== 'object' ||
    mcp === null ||
    Array.isArray(mcp)
  ) {
    return { text, changed: false, actions };
  }

  const servers = mcp as Record<string, unknown>;

  // Step 1: identify the coord entry. Rename it to `st` iff no `st`
  // entry already exists; if both exist, prefer the existing `st`
  // (avoid clobbering hand-tuned config) and just drop the `coord`
  // key.
  let coordEntry: Record<string, unknown> | null = null;
  if (Object.prototype.hasOwnProperty.call(servers, 'coord')) {
    const raw = servers.coord;
    if (typeof raw === 'object' && raw !== null && !Array.isArray(raw)) {
      coordEntry = raw as Record<string, unknown>;
    }
  }

  if (coordEntry !== null) {
    if (Object.prototype.hasOwnProperty.call(servers, 'st')) {
      delete servers.coord;
      actions.push(
        'dropped mcpServers.coord (mcpServers.st already present)'
      );
    } else {
      delete servers.coord;
      servers.st = coordEntry;
      actions.push('renamed mcpServers.coord → mcpServers.st');
    }
  }

  // Step 2+3+4: process each server entry (both a freshly-renamed
  // `st` and any pre-existing entries). The path/env rewrites are
  // idempotent so applying them to already-migrated entries is safe.
  for (const key of Object.keys(servers)) {
    const entry = servers[key];
    if (typeof entry !== 'object' || entry === null || Array.isArray(entry)) {
      continue;
    }
    const e = entry as Record<string, unknown>;
    rewriteCommand(e, key, actions);
    rewriteEnv(e, key, actions);
  }

  const changed = actions.length > 0;
  if (!changed) return { text, changed: false, actions };

  // Preserve trailing newline shape from the input.
  const trailingNewline = text.endsWith('\n');
  const out = JSON.stringify(obj, null, 2) + (trailingNewline ? '\n' : '');
  return { text: out, changed, actions };
}

function rewriteCommand(
  entry: Record<string, unknown>,
  key: string,
  actions: string[]
): void {
  const cmd = entry.command;
  if (typeof cmd !== 'string' || cmd.length === 0) return;
  // Legal cutover targets. The suffix `myobie/smalltalk/bin/st` is
  // the terminal state; anything else that names a coord binary path
  // gets rewritten to it.
  const rewrites: [RegExp, string][] = [
    [/myobie\/coord\/bin\/coord$/, 'myobie/smalltalk/bin/st'],
    [/myobie\/smalltalk\/bin\/coord$/, 'myobie/smalltalk/bin/st'],
    [/myobie\/coord\/bin\/st$/, 'myobie/smalltalk/bin/st'],
    [/myobie\/coord\/bin\/smalltalk$/, 'myobie/smalltalk/bin/st'],
  ];
  for (const [re, dst] of rewrites) {
    if (re.test(cmd)) {
      const next = cmd.replace(re, dst);
      if (next !== cmd) {
        entry.command = next;
        actions.push(
          `mcpServers.${key}.command: ${cmd} → ${next}`
        );
        return;
      }
    }
  }
}

function rewriteEnv(
  entry: Record<string, unknown>,
  key: string,
  actions: string[]
): void {
  const env = entry.env;
  if (typeof env !== 'object' || env === null || Array.isArray(env)) {
    return;
  }
  const e = env as Record<string, unknown>;

  // Rename pairs, driven off the cutover map. The `dropAlias` list is
  // legacy aliases we drop when the canonical name is set (post-P1
  // agents may have set both belt-and-suspenders).
  const renames: [from: string, to: string, dropAlias: string[]][] = [
    ['COORD_IDENTITY', 'ST_AGENT', ['ST_IDENTITY']],
    ['COORD_ROOT', 'ST_ROOT', []],
    ['COORD_CONFIG', 'ST_CONFIG', []],
  ];
  for (const [from, to, dropAliases] of renames) {
    const legacyHas = Object.prototype.hasOwnProperty.call(e, from);
    const canonicalHas = Object.prototype.hasOwnProperty.call(e, to);
    if (legacyHas && canonicalHas) {
      // Both set — legacy is redundant. Drop it.
      delete e[from];
      actions.push(
        `mcpServers.${key}.env: dropped redundant ${from} (${to} already set)`
      );
    } else if (legacyHas) {
      const val = e[from];
      delete e[from];
      e[to] = val;
      actions.push(
        `mcpServers.${key}.env: renamed ${from} → ${to}`
      );
    }
    for (const alias of dropAliases) {
      if (
        Object.prototype.hasOwnProperty.call(e, alias) &&
        Object.prototype.hasOwnProperty.call(e, to)
      ) {
        delete e[alias];
        actions.push(
          `mcpServers.${key}.env: dropped redundant ${alias} (${to} already set)`
        );
      }
    }
  }
}
