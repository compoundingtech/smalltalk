// commands/completions.ts — print a shell completion script for `coord`.
//
// coord dispatches subcommands via a hand-written `switch` in cli.ts and
// its `--help` output is prose, so there is no machine-readable command
// table to derive completions from. Instead this module owns a small
// declarative spec of the command tree (groups → verbs → flags, plus the
// enum-valued flags and positionals) and generates fish / bash / zsh from
// that ONE spec, so the three scripts can't drift apart.
//
// Enum binding is per-command-node, not per-flag-name: `--status` resolves
// to different value sets depending on context (settable states under
// `status --set` / `members --status`), so the values live on the node
// that owns the flag.

import type { CliContext } from '../cli-context.ts';
import { SETTABLE_STATES } from '../common.ts';
import { PRIORITIES } from '../types.ts';

// ─── Spec ──────────────────────────────────────────────────────────────────
//
// The closed enums are imported from their source-of-truth modules rather
// than re-typed here, so a change to the canonical list flows into every
// generated script automatically.

const STATE_VALUES = SETTABLE_STATES.map(String);
const PRIORITY_VALUES = PRIORITIES.map(String);

/** A `--flag`. `values` (when present) is the closed set of completions for
 *  the flag's argument; absence means a boolean flag or a free-form value. */
interface FlagSpec {
  readonly name: string;
  readonly desc: string;
  readonly values?: readonly string[];
}

/** A leaf command: a top-level subcommand, or a verb under a group. */
interface CommandSpec {
  readonly name: string;
  readonly desc: string;
  /** Aliases for the command name (e.g. `msg` for `message`). */
  readonly aliases?: readonly string[];
  /** Nested verbs (e.g. `send` under `message`). */
  readonly verbs?: readonly CommandSpec[];
  /** Flags accepted directly by this command/verb. */
  readonly flags?: readonly FlagSpec[];
  /** A closed set of values for a positional argument (e.g. the STATE in
   *  `task status <filename> <STATE>`). */
  readonly positionalValues?: readonly string[];
  /** Whether this command takes a file/path positional (offer file
   *  completion, e.g. `init [<dir>]`). */
  readonly takesPath?: boolean;
}

const JSON_FLAG: FlagSpec = { name: 'json', desc: 'JSON output' };

/**
 * The coord command tree. Keep in sync with the `switch` dispatch in
 * cli.ts and the per-command flag parsing in src/commands/*.ts.
 */
const COMMANDS: readonly CommandSpec[] = [
  {
    name: 'message',
    aliases: ['msg'],
    desc: 'Send/list/read/archive messages',
    verbs: [
      {
        name: 'send',
        desc: 'Send a message (body from stdin)',
        flags: [
          { name: 'from', desc: 'Sender identity' },
          { name: 'subject', desc: 'Subject line' },
          { name: 'in-reply-to', desc: 'Reply to inbox filename' },
          { name: 'tags', desc: 'Comma-separated tags' },
          { name: 'priority', desc: 'Message priority', values: PRIORITY_VALUES },
        ],
      },
      {
        name: 'ls',
        desc: 'List messages',
        flags: [
          { name: 'archive', desc: 'List archived messages' },
          { name: 'count', desc: 'Print count only' },
          { name: 'from', desc: 'Filter by sender' },
          { name: 'since', desc: 'Only since UNIX_MS' },
          { name: 'orphans', desc: 'List orphan prefix-sibling attachments' },
          JSON_FLAG,
        ],
      },
      {
        name: 'read',
        desc: 'Read a message',
        flags: [
          { name: 'raw', desc: 'Print raw message' },
          { name: 'archive', desc: 'Read from archive' },
          JSON_FLAG,
        ],
      },
      {
        // `archive <filename>` and `archive trim [--older-than ...]`. The
        // trim-only flags (`--older-than`/`--keep-last`/`--dry-run`) only
        // apply after the `trim` token, so they're intentionally not
        // offered on bare `archive` to avoid completing them one level
        // early. `--with-attachments` (issue #8) is shared between the two.
        name: 'archive',
        desc: 'Archive (or trim archived) messages',
        flags: [
          {
            name: 'with-attachments',
            desc: 'Also move/trim prefix-sibling attachments',
          },
        ],
      },
      {
        name: 'thread',
        desc: 'Show a message thread',
        flags: [{ name: 'tree', desc: 'Render thread as a tree' }],
      },
    ],
  },
  {
    name: 'watch',
    desc: 'Live-watch an inbox',
    flags: [
      { name: 'all', desc: 'Watch cross-tree' },
      { name: 'with-subject', desc: 'Show subjects' },
      { name: 'since', desc: 'Only since UNIX_MS' },
      { name: 'since-now', desc: 'Only new arrivals' },
      { name: 'interval', desc: 'Poll interval (ms)' },
      { name: 'once', desc: 'Render once and exit' },
    ],
  },
  {
    name: 'status',
    desc: 'Show or set member status',
    flags: [{ name: 'set', desc: 'Set your status', values: STATE_VALUES }],
  },
  {
    name: 'members',
    desc: 'List members',
    flags: [
      { name: 'status', desc: 'Filter by status', values: STATE_VALUES },
      { name: 'enrich', desc: 'Enrich JSON output' },
      JSON_FLAG,
    ],
  },
  {
    name: 'overview',
    desc: 'Tree overview of recent activity',
    flags: [{ name: 'recent', desc: 'Number of recent items' }, JSON_FLAG],
  },
  {
    name: 'resource',
    desc: 'Manage annotated URLs you publish (brief-009 item 5)',
    verbs: [
      {
        name: 'add',
        desc: 'Add a resource (a URL with optional title/tags/relation/body)',
        flags: [
          { name: 'title', desc: 'One-line title' },
          { name: 'tag', desc: 'Comma-separated tags' },
          {
            name: 'relation',
            desc: 'Free-form relation (canonical: owns | relates-to | depends-on)',
          },
          { name: 'body-stdin', desc: 'Read body description from stdin' },
        ],
      },
      {
        name: 'ls',
        desc: 'List resources for an identity',
        flags: [JSON_FLAG],
      },
      {
        name: 'read',
        desc: 'Read one resource',
        flags: [JSON_FLAG],
      },
      { name: 'rm', desc: 'Remove one of your own resources' },
    ],
  },
  {
    name: 'sync',
    desc: 'Push/pull against peers',
    verbs: [
      { name: 'push', desc: 'Push to a peer' },
      { name: 'pull', desc: 'Pull from a peer' },
      { name: 'sweep', desc: 'Enforce the LAYOUT tombstone invariant' },
    ],
    flags: [{ name: 'all', desc: 'Apply against every peer' }],
  },
  { name: 'mcp', desc: 'Run as an MCP stdio server' },
  {
    name: 'init',
    desc: 'Write/merge .mcp.json so Claude loads coord',
    takesPath: true,
    flags: [
      { name: 'no-channel', desc: 'Skip channel setup' },
      { name: 'print', desc: 'Print instead of writing' },
      { name: 'force', desc: 'Overwrite existing config' },
    ],
  },
  {
    name: 'launch',
    desc: 'One-command harness bootstrap onto smalltalk',
    positionalValues: ['claude', 'codex'],
    flags: [
      { name: 'identity', desc: 'Explicit agent name' },
      { name: 'model', desc: 'Ollama model spec (routes via `ollama launch`)' },
      { name: 'no-pty', desc: "Don't register via pty even if it's on PATH" },
      { name: 'no-channel', desc: 'Skip --channel MCP wiring' },
      { name: 'session-name', desc: 'Override pty session key' },
      { name: 'dry-run', desc: 'Print what would happen; touch nothing' },
      { name: 'print', desc: 'Alias for --dry-run' },
    ],
  },
  { name: 'ding', desc: 'Busy-aware push notifier' },
  {
    name: 'completions',
    desc: 'Print a shell completion script',
    positionalValues: ['fish', 'bash', 'zsh'],
  },
];

/** Every spelling (name + aliases) of every top-level subcommand. */
const allCommandNames = (): readonly string[] =>
  COMMANDS.flatMap((c) => [c.name, ...(c.aliases ?? [])]);

// ─── fish ──────────────────────────────────────────────────────────────────

/** All name spellings of a command, space-joined, for fish guards. */
const fishNames = (c: CommandSpec): string =>
  [c.name, ...(c.aliases ?? [])].join(' ');

function fishScript(): string {
  const out: string[] = [];
  out.push('# fish completions for coord — generated by `coord completions fish`.');
  out.push('# Regenerate with: coord completions fish > ~/.config/fish/completions/coord.fish');
  out.push('');
  out.push('complete -c coord -e');
  out.push('complete -c coord -f');
  out.push('');
  out.push('# Top-level subcommands');
  for (const c of COMMANDS) {
    for (const name of [c.name, ...(c.aliases ?? [])]) {
      out.push(
        `complete -c coord -n __fish_use_subcommand -a ${name} -d ${q(c.desc)}`
      );
    }
  }

  for (const c of COMMANDS) {
    const verbs = c.verbs ?? [];
    const verbNames = verbs.map((v) => v.name).join(' ');
    if (verbs.length > 0) {
      out.push('');
      out.push(`# ${c.name} verbs`);
      // A verb is offered only once the group is seen and no verb yet is.
      const guard = `__fish_seen_subcommand_from ${fishNames(c)}; and not __fish_seen_subcommand_from ${verbNames}`;
      for (const v of verbs) {
        out.push(
          `complete -c coord -n ${q(guard)} -a ${v.name} -d ${q(v.desc)}`
        );
      }
      // Per-verb flags + positionals. Verb names like `ls`/`new`/`status`
      // collide across groups, so guard on BOTH the verb and the group.
      for (const v of verbs) {
        fishEmitForNode(out, v, [
          `__fish_seen_subcommand_from ${v.name}`,
          `__fish_seen_subcommand_from ${fishNames(c)}`,
        ]);
      }
    }
    // Flags that live directly on the group/command (no verb).
    fishEmitForNode(out, c, [`__fish_seen_subcommand_from ${fishNames(c)}`]);
  }

  return out.join('\n') + '\n';
}

/** Emit fish `complete` lines for a node's flags and positional values,
 *  gated by `guards` (all must hold). */
function fishEmitForNode(
  out: string[],
  node: CommandSpec,
  guards: readonly string[]
): void {
  const cond = guards.join('; and ');
  for (const f of node.flags ?? []) {
    if (f.values) {
      out.push(
        `complete -c coord -n ${q(cond)} -l ${f.name} -x -a ${q(f.values.join(' '))} -d ${q(f.desc)}`
      );
    } else {
      out.push(`complete -c coord -n ${q(cond)} -l ${f.name} -d ${q(f.desc)}`);
    }
  }
  if (node.positionalValues) {
    out.push(
      `complete -c coord -n ${q(cond)} -x -a ${q(node.positionalValues.join(' '))} -d ${q('Value')}`
    );
  }
  if (node.takesPath) {
    out.push(`complete -c coord -n ${q(cond)} -F`);
  }
}

/** Single-quote a string for fish (fish only special-cases `'` and `\`). */
function q(s: string): string {
  return `'${s.replace(/\\/g, '\\\\').replace(/'/g, "\\'")}'`;
}

// ─── bash ────────────────────────────────────────────────────────────────────
//
// A flat completer: at depth 1 offer subcommands; once a known group is the
// first word, offer its verbs and flags. Enum flags complete their values
// when the previous word is the flag. Behavioral parity with fish is a
// non-goal here (the task only requires fish behaviorally); this gives
// useful subcommand+flag completion and is syntactically sourceable.

function bashScript(): string {
  const tops = allCommandNames().join(' ');
  const lines: string[] = [];
  const groups = COMMANDS.filter((c) => (c.verbs?.length ?? 0) > 0);

  // Per-group: "<verbs> <group-flags>" candidate strings.
  const groupCandidates = groups
    .flatMap((c) => [c.name, ...(c.aliases ?? [])].map((n) => ({ n, c })))
    .map(({ n, c }) => {
      const verbs = (c.verbs ?? []).map((v) => v.name);
      const flags = (c.flags ?? []).map((f) => `--${f.name}`);
      return `    ${n}) words="${[...verbs, ...flags].join(' ')}" ;;`;
    });

  // Per-leaf-command (no verbs): its flags.
  const leafCandidates = COMMANDS.filter((c) => (c.verbs?.length ?? 0) === 0)
    .flatMap((c) => [c.name, ...(c.aliases ?? [])].map((n) => ({ n, c })))
    .filter(({ c }) => (c.flags?.length ?? 0) > 0)
    .map(({ n, c }) => {
      const flags = (c.flags ?? []).map((f) => `--${f.name}`);
      return `    ${n}) words="${flags.join(' ')}" ;;`;
    });

  // Enum value sets, keyed by "--flag" only where unambiguous globally is
  // false, so key by previous word + nearest known enum. Simplest correct
  // behavior: map each flag spelling to the union is wrong, so instead emit
  // a case on the previous word scoped by the first word.
  const enumCases = bashEnumCases();

  lines.push('# bash completion for coord — generated by `coord completions bash`.');
  lines.push('# Regenerate with: coord completions bash > /etc/bash_completion.d/coord');
  lines.push('_coord() {');
  lines.push('  local cur prev words cword');
  lines.push('  COMPREPLY=()');
  lines.push('  cur="${COMP_WORDS[COMP_CWORD]}"');
  lines.push('  prev="${COMP_WORDS[COMP_CWORD-1]}"');
  lines.push('  local cmd="${COMP_WORDS[1]}"');
  lines.push('  local verb="${COMP_WORDS[2]}"');
  lines.push('');
  lines.push('  # Enum-valued flags / positionals complete their value set.');
  lines.push(enumCases);
  lines.push('');
  lines.push('  # First word: top-level subcommands.');
  lines.push('  if [ "$COMP_CWORD" -eq 1 ]; then');
  lines.push(`    COMPREPLY=( $(compgen -W "${tops}" -- "$cur") )`);
  lines.push('    return 0');
  lines.push('  fi');
  lines.push('');
  lines.push('  # Group verbs + flags.');
  lines.push('  local words=""');
  lines.push('  case "$cmd" in');
  lines.push(...groupCandidates);
  lines.push(...leafCandidates);
  lines.push('    *) words="" ;;');
  lines.push('  esac');
  lines.push('  if [ -n "$words" ]; then');
  lines.push('    COMPREPLY=( $(compgen -W "$words" -- "$cur") )');
  lines.push('  fi');
  lines.push('  return 0');
  lines.push('}');
  lines.push('complete -F _coord coord');
  return lines.join('\n') + '\n';
}

/** Build the bash `case "$prev"` block that completes enum flag values,
 *  scoped by the first word where needed. */
function bashEnumCases(): string {
  const lines: string[] = [];
  // Flag-value enums, scoped by cmd where needed.
  // status --set → settable; members --status → settable.
  // priorities are unambiguous wherever they appear.
  lines.push('  case "$prev" in');
  lines.push(`    --set) COMPREPLY=( $(compgen -W "${STATE_VALUES.join(' ')}" -- "$cur") ); return 0 ;;`);
  lines.push(`    --priority) COMPREPLY=( $(compgen -W "${PRIORITY_VALUES.join(' ')}" -- "$cur") ); return 0 ;;`);
  lines.push('    --status)');
  lines.push('      case "$cmd" in');
  lines.push(`        members) COMPREPLY=( $(compgen -W "${STATE_VALUES.join(' ')}" -- "$cur") ) ;;`);
  lines.push('      esac');
  lines.push('      return 0 ;;');
  lines.push('  esac');
  return lines.join('\n');
}

// ─── zsh ─────────────────────────────────────────────────────────────────────
//
// A `#compdef`-style function. Like bash, behavioral parity with fish is a
// non-goal — this offers subcommands, verbs, flags, and enum value sets,
// and is syntactically sourceable (`zsh -n`).

function zshScript(): string {
  const tops = allCommandNames().join(' ');
  const groups = COMMANDS.filter((c) => (c.verbs?.length ?? 0) > 0);
  const lines: string[] = [];

  lines.push('#compdef coord');
  lines.push('# zsh completion for coord — generated by `coord completions zsh`.');
  lines.push('# Regenerate with: coord completions zsh > "${fpath[1]}/_coord"');
  lines.push('_coord() {');
  lines.push('  local cmd="${words[2]}" verb="${words[3]}" prev="${words[CURRENT-1]}"');
  lines.push('');
  lines.push('  case "$prev" in');
  lines.push(`    --set) compadd ${STATE_VALUES.join(' ')}; return ;;`);
  lines.push(`    --priority) compadd ${PRIORITY_VALUES.join(' ')}; return ;;`);
  lines.push('    --status)');
  lines.push('      case "$cmd" in');
  lines.push(`        members) compadd ${STATE_VALUES.join(' ')} ;;`);
  lines.push('      esac');
  lines.push('      return ;;');
  lines.push('  esac');
  lines.push('');
  lines.push('  if [ "$CURRENT" -eq 2 ]; then');
  lines.push(`    compadd ${tops}; return`);
  lines.push('  fi');
  lines.push('');
  lines.push('  case "$cmd" in');
  for (const c of groups) {
    for (const n of [c.name, ...(c.aliases ?? [])]) {
      const verbs = (c.verbs ?? []).map((v) => v.name);
      const flags = (c.flags ?? []).map((f) => `--${f.name}`);
      lines.push(`    ${n}) compadd ${[...verbs, ...flags].join(' ')} ;;`);
    }
  }
  for (const c of COMMANDS.filter((c) => (c.verbs?.length ?? 0) === 0)) {
    const flags = (c.flags ?? []).map((f) => `--${f.name}`);
    if (flags.length === 0) continue;
    for (const n of [c.name, ...(c.aliases ?? [])]) {
      lines.push(`    ${n}) compadd ${flags.join(' ')} ;;`);
    }
  }
  lines.push('  esac');
  lines.push('}');
  lines.push('_coord "$@"');
  return lines.join('\n') + '\n';
}

// ─── Dispatch ────────────────────────────────────────────────────────────────

const GENERATORS: Record<string, () => string> = {
  fish: fishScript,
  bash: bashScript,
  zsh: zshScript,
};

const SHELLS = Object.keys(GENERATORS);

const USAGE =
  `usage: coord completions <shell>\n\n` +
  `Print a shell completion script to stdout.\n\n` +
  `Shells:\n` +
  SHELLS.map((s) => `  ${s}`).join('\n') +
  `\n\nExamples:\n` +
  `  coord completions fish > ~/.config/fish/completions/coord.fish\n` +
  `  coord completions bash > /etc/bash_completion.d/coord\n` +
  `  coord completions zsh  > "\${fpath[1]}/_coord"\n`;

/**
 * `coord completions <shell>` — write a completion script for `shell` to
 * stdout. Unknown or missing shell prints usage to stderr and returns 2
 * (the CLI's usage-error code).
 */
export function cmdCompletionsCli(
  args: readonly string[],
  ctx: CliContext
): number {
  const shell = args[0];
  if (shell === undefined || shell === '--help' || shell === '-h') {
    // `--help`/`-h` is an explicit request → stdout, exit 0.
    if (shell === '--help' || shell === '-h') {
      ctx.stdout(USAGE);
      return 0;
    }
    ctx.stderr(USAGE);
    return 2;
  }
  const gen = GENERATORS[shell];
  if (gen === undefined) {
    ctx.stderr(`coord completions: unknown shell: ${shell}\n\n`);
    ctx.stderr(USAGE);
    return 2;
  }
  ctx.stdout(gen());
  return 0;
}

// Exposed for tests.
export { COMMANDS, SHELLS, fishScript, bashScript, zshScript };
