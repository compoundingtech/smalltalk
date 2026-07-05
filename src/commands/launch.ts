// commands/launch.ts — `smalltalk launch <claude|codex>` verb.
//
// One-command harness bootstrap onto smalltalk. Wires up identity +
// .mcp.json + (harness-specific) session-id + pty registration and
// hands off to the harness binary. Shaped like `ollama launch`.
//
// brief-016 (smalltalk launch): getting a non-Claude harness cleanly
// onto smalltalk previously took multiple fixes; GLM/ollama had an
// unattended-launch gap (interactive model picker). This verb closes
// both. See `notes/harness-integrations.md` for the individual harness
// mechanics — this file is the composition layer.
//
// Design decisions (surfaced in the PR desc so cos can walk):
//   - --model routes through `ollama launch <harness> --model <spec>`.
//     Chose ollama's --model flag over env-extraction: cleaner UX,
//     less brittle.
//   - Skip-if-exists on .claude-session-id and pty.toml. --force to
//     overwrite (never generated a --force flag; the safer default
//     is what we ship). Users can `rm` these to re-bootstrap.
//   - Bare harness name as pty session key (`claude` / `codex`);
//     pty.toml's `prefix` field handles the `<repo>-` prefix.
//   - claude defaults to channel mode (has UI for it); codex defaults
//     to non-channel + a `coord ding` sidecar (no asyncRewake).

import { spawn, spawnSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { homedir } from 'node:os';
import { dirname, isAbsolute, join, resolve } from 'node:path';

import type { CliContext } from '../cli-context.ts';
import {
  coordRootFrom,
  ensureIdentityDirs,
  envAgentFrom,
  rand6,
} from '../common.ts';
import { cmdInit, resolveCoordBinPath } from './init.ts';

// ─── Shape ──────────────────────────────────────────────────────────────

export type Harness = 'claude' | 'codex';

export interface LaunchInput {
  harness: Harness;
  /** Override the resolved identity. */
  identity?: string | undefined;
  /**
   * When set, route the launch through `ollama launch <harness>
   * --model <spec>` so ollama does the env injection + skips its
   * interactive model picker. Path to unattended GLM-backed agents.
   */
  model?: string | undefined;
  /** When true, don't register via pty even if `pty` is on PATH. */
  noPty?: boolean | undefined;
  /** When true, skip `--channel` on the MCP wiring. Default for claude
   *  is channel-on; for codex is channel-off (codex has no channel UI). */
  noChannel?: boolean | undefined;
  /** Override the pty session name (default: harness name). */
  sessionName?: string | undefined;
  /** When true, print what would happen; touch nothing on disk / no
   *  process spawn. */
  dryRun?: boolean | undefined;

  /**
   * brief-023: claude `--permission-mode` value. Threaded into BOTH the
   * bootstrap argv (`claude --print --permission-mode <mode> …`) and
   * the main argv (`claude --permission-mode <mode> … --resume`), and
   * baked into the generated `pty.toml` command line. Precedence at
   * resolution time: explicit flag > env var `CLAUDE_PERMISSION_MODE`
   * (parity with `pty-claude-launcher.sh`) > default `auto`. Values are
   * passed through to claude verbatim (`acceptEdits`, `auto`,
   * `bypassPermissions`, `default`, `dontAsk`, `plan`); no validation
   * here — claude rejects unknown modes loudly enough that duplicating
   * the enum here would just rot. Codex ignores this — the codex
   * harness has its own approval-policy surface; silently no-ops for
   * codex launches.
   */
  permissionMode?: string | undefined;
  /**
   * brief-022: install a persona alongside the harness. The path is
   * copied to `<cwd>/PERSONA.md`, and the harness entry file
   * (`CLAUDE.md` for claude, `AGENTS.md` for codex) gets a
   * `@PERSONA.md` import line surgically appended if not already
   * present. Both harnesses support the `@`-import mechanism
   * (verified 2026-07-02 against codex 0.142.4 + claude 2.1.198).
   * We git-exclude the infra we generate via `.git/info/exclude` so
   * the persona doesn't pollute the target repo's commits.
   *
   * When the entry file didn't exist and we created it, that file
   * gets excluded too; when it pre-existed (a real repo CLAUDE.md /
   * AGENTS.md), it's left in the repo untouched — only the appended
   * line adds an ignored import.
   */
  persona?: string | undefined;
  /**
   * The binary name to invoke as the harness executable. Only affects
   * the `claude` harness — codex has its own launcher and is untouched.
   * Defaults to `'claude'`; callers can pass an alias (`cl1`, `cl2`,
   * `claude-preview`, etc.) so aliased shells find the right binary.
   * Resolution at CLI level: `--agent` flag > `$AGENT` env > `'claude'`
   * default. Threaded into BOTH the bootstrap argv
   * (`<agent> --print --session-id …`) and the main argv
   * (`<agent> --resume …`), and baked into the generated `pty.toml`
   * command line so the pty-spawned process invokes the alias too.
   *
   * The pty `session-name` stays independent of this — it defaults to
   * the harness kind (`claude`) so the pty layout stays consistent
   * even when the underlying binary is `cl1`. Override via
   * `--session-name` if you want a different session key.
   */
  agentBinary?: string | undefined;

  /** Working directory. Default: process.cwd(). */
  cwd?: string | undefined;
  /** Test seam: override the pty detection. */
  ptyBinPath?: string | undefined;
  /** Test seam: override HOME. */
  home?: string | undefined;
  /** Test seam: skip the actual spawn; return the constructed argv. */
  captureOnly?: boolean | undefined;
  /**
   * brief-118 test seam: override the location of the shipped Claude
   * Code hook scripts (`examples/claude-code/hooks/`). Production
   * resolves this from the smalltalk repo root via
   * `resolveCoordBinPath()`. Tests set this to a real path so the
   * generated `settings.local.json` references stable content
   * regardless of where the smalltalk checkout lives on the runner.
   */
  hooksDir?: string | undefined;
  /**
   * Test seam for the ST_BIN injection into hook `command:` strings.
   * Production resolves this via `resolveStBinPath(resolveCoordBinPath())`
   * so the hooks fire the same binary the operator ran `st launch`
   * with, regardless of PATH state at hook-execution time.
   *
   * - `undefined` (default): auto-resolve.
   * - `null`: explicitly opt out — no `ST_BIN=` prefix injected into
   *   the hook commands. The generator emits bare script paths, and
   *   the hook scripts fall back to their internal
   *   `command -v st || command -v coord` PATH lookup at runtime.
   * - a string: use that path verbatim. Tests pass a synthetic path
   *   so the generated JSON is stable across dev machines.
   */
  stBinForHooks?: string | null | undefined;
  /**
   * brief-118: skip generating `.claude/settings.local.json` even for
   * the claude harness. Codex launches implicitly skip. Off by default
   * so `st launch claude` opts every new agent into the SessionStart
   * asyncRewake + PreCompact flush + StopFailure hooks by default.
   */
  noHooks?: boolean | undefined;

  env: NodeJS.ProcessEnv;
  coordRoot: string;
}

export interface LaunchResult {
  identity: string;
  identityAutoGenerated: boolean;
  channel: boolean;
  usedPty: boolean;
  usedOllama: boolean;
  /** The argv the underlying process was (or would be) launched with. */
  argv: readonly string[];
  /** Path to the .mcp.json that was written (or would be). */
  mcpJsonPath: string;
  /** Path to pty.toml if we wrote one, else null. */
  ptyTomlPath: string | null;
  /** Path to .claude-session-id if we wrote one, else null. */
  claudeSessionIdPath: string | null;
  /** Text of pty.toml when running --dry-run, else null. */
  ptyTomlPreview: string | null;
  /**
   * brief-023: the resolved `--permission-mode` value that was baked
   * into the claude argv + pty.toml. Populated regardless of harness so
   * the dry-run summary can surface it consistently; for codex this
   * reflects the resolution but doesn't feed the argv (codex has its
   * own approval-policy surface).
   */
  permissionMode: string;
  /**
   * The binary that was (or would be) invoked as argv[0] for the claude
   * harness. Populated for both harnesses so the dry-run summary can
   * report it consistently; for codex this reflects the resolution but
   * doesn't feed the codex argv (codex has its own launcher). Default
   * `'claude'` when no `--agent` flag / `$AGENT` env is set.
   */
  agentBinary: string;
  /**
   * brief-118: absolute path to `.claude/settings.local.json` when the
   * launch generated (or would have generated) one. Non-null only for
   * the claude harness with `noHooks !== true` AND with a resolvable
   * hooks directory on disk. `null` when we skipped for any reason
   * (codex harness, `--no-hooks`, `noHooks: true`, or the shipped
   * `examples/claude-code/hooks/` directory couldn't be located).
   */
  claudeSettingsPath: string | null;
  /**
   * brief-118: text of the generated `settings.local.json` — populated
   * on --dry-run so the summary can display it, populated on live
   * runs iff we actually wrote the file. `null` when we skipped.
   */
  claudeSettingsPreview: string | null;
  /**
   * brief-022: summary of the persona install, or null when
   * `--persona` was not passed. Populated in both real-run and
   * `--dry-run` mode so tests + CLI dry-run output can inspect the
   * decisions we would make without touching disk.
   */
  persona: PersonaInstallResult | null;
}

/**
 * brief-022: what {@link cmdLaunch} decided to do with the persona
 * install. All fields are populated regardless of --dry-run — a
 * dry-run just skips the actual file I/O and git-exclude append. The
 * `entryFileCreated` flag drives the exclusion decision: only files we
 * created get excluded, so a pre-existing repo CLAUDE.md never gets
 * added to the ignore list.
 */
export interface PersonaInstallResult {
  /** Path we would (or did) copy the persona source to. */
  personaMdPath: string;
  /** `CLAUDE.md` (claude harness) or `AGENTS.md` (codex harness). */
  entryFile: 'CLAUDE.md' | 'AGENTS.md';
  /** Absolute path to the entry file. */
  entryFilePath: string;
  /** True when the entry file didn't exist and we created it. */
  entryFileCreated: boolean;
  /** True when we appended a `@PERSONA.md` line (vs. it was already
   *  present). */
  importLineAppended: boolean;
  /** Entries that were (or would be) appended to `.git/info/exclude`. */
  gitExcludeEntriesAdded: readonly string[];
  /** True when the target cwd was not a git repo, so we couldn't
   *  exclude. Callers can surface a warning; the persona still
   *  installs. */
  gitRepoAbsent: boolean;
}

// ─── Helpers ────────────────────────────────────────────────────────────

const ANON_PREFIX = 'anon-';

function generateAnonAgent(): string {
  return `${ANON_PREFIX}${rand6()}`;
}

function detectPtyPath(explicitOverride?: string | undefined): string | null {
  if (explicitOverride !== undefined) {
    return explicitOverride.length > 0 ? explicitOverride : null;
  }
  try {
    const r = spawnSync('which', ['pty'], { encoding: 'utf8' });
    if (r.status === 0 && typeof r.stdout === 'string') {
      const found = r.stdout.trim();
      if (found.length > 0 && existsSync(found)) return found;
    }
  } catch {
    // ignore
  }
  return null;
}

/**
 * Encode a working directory the same way Claude Code does when it
 * decides where under `~/.claude/projects/` to write its jsonl.
 * `pty-claude-launcher.sh` uses `tr '/.' '--'`; we mirror it.
 */
function encodedCwd(cwd: string): string {
  return cwd.replace(/[/.]/g, '-');
}

function newUuid(): string {
  // Use node:crypto's randomUUID rather than shelling out to uuidgen
  // (which the reference `pty-claude-launcher.sh` does — we avoid the
  // extra subprocess). Imported statically at the top of this module
  // because the file runs under ESM (`"type": "module"` + `node
  // --experimental-strip-types`) and CJS `require()` is unavailable.
  return randomUUID();
}

// ─── Command construction ──────────────────────────────────────────────

/**
 * Build the claude launch argv, mirroring pty-claude-launcher.sh's
 * jsonl-bootstrap + `--resume <SID>` handoff. When `bootstrapWithPrint`
 * is true, the *caller* is responsible for running a one-shot
 * `claude --print --session-id <SID> "session init"` before the main
 * argv — we return both parts.
 */
function buildClaudeCommand(opts: {
  cwd: string;
  home: string;
  channel: boolean;
  claudeSessionId: string;
  permissionMode: string;
  /** The binary to invoke as argv[0]. See {@link LaunchInput.agentBinary}. */
  agentBinary: string;
}): {
  bootstrapArgv: readonly string[];
  mainArgv: readonly string[];
  jsonlPath: string;
} {
  const encoded = encodedCwd(opts.cwd);
  const jsonlPath = join(
    opts.home,
    '.claude',
    'projects',
    encoded,
    `${opts.claudeSessionId}.jsonl`
  );
  const channelFlag = opts.channel
    ? ['--dangerously-load-development-channels', 'server:st']
    : [];
  const mainArgv: readonly string[] = [
    opts.agentBinary,
    '--permission-mode',
    opts.permissionMode,
    ...channelFlag,
    '--resume',
    opts.claudeSessionId,
  ];
  // Bootstrap fires only when the jsonl is missing; caller checks
  // `existsSync(jsonlPath)`.
  const bootstrapArgv: readonly string[] = [
    opts.agentBinary,
    '--print',
    '--permission-mode',
    opts.permissionMode,
    '--session-id',
    opts.claudeSessionId,
    'session init',
  ];
  return { bootstrapArgv, mainArgv, jsonlPath };
}

function buildOllamaCommand(
  harness: Harness,
  model: string
): readonly string[] {
  return ['ollama', 'launch', harness, '--model', model];
}

function buildCodexCommand(cwd: string): readonly string[] {
  const idFile = join(cwd, '.codex-session-id');
  if (existsSync(idFile)) {
    const sid = readFileSync(idFile, 'utf8').trim();
    if (sid.length > 0) return ['codex', 'resume', sid];
  }
  return ['codex'];
}

// ─── pty.toml generation ───────────────────────────────────────────────

/**
 * Quote a single argv element for a shell command line. Elements with
 * no unsafe chars pass through bare; anything else gets single-quoted
 * with embedded single quotes escaped as `'\''`. Not exhaustive but
 * safe for our known argv shapes (harness flags + UUID + model spec).
 */
function shellQuote(a: string): string {
  if (/^[a-zA-Z0-9_\-./:@=+,]+$/.test(a)) return a;
  return `'${a.replace(/'/g, `'\\''`)}'`;
}

/**
 * TOML-escape a bare string for insertion into a `"..."` value. TOML
 * requires backslash-escapes for `\"` and `\\`, and permits others.
 */
function tomlEscape(s: string): string {
  return s.replace(/\\/g, '\\\\').replace(/"/g, '\\"');
}

function buildPtyToml(opts: {
  harness: Harness;
  sessionName: string;
  identity: string;
  invocation: readonly string[];
  addDingSidecar: boolean;
}): string {
  // pty runs `command` via `sh -c`, so produce a shell command line by
  // space-joining shell-quoted argv elements. Then TOML-quote the
  // whole thing for the `command = "..."` value.
  const shellLine = opts.invocation.map(shellQuote).join(' ');
  // Prefix is the identity — pty namespace is global and repo
  // basenames can collide (`taskflow` in two different clones both
  // trying to be `taskflow/claude`). Identity is unique per agent by
  // construction, and it means a generic shepherd poking `pty send
  // <identity>/<session>` always finds the right session. Historic
  // behavior was repo-basename via a `resolveRepoPrefix(cwd)` walk;
  // that shape is preserved by `--session-name` when a user really
  // wants a different key inside the identity namespace.
  const prefix = opts.identity;
  const lines: string[] = [];
  lines.push(`prefix = "${tomlEscape(prefix)}"`);
  lines.push('');
  lines.push(`[sessions.${opts.sessionName}]`);
  lines.push(`command = "${tomlEscape(shellLine)}"`);
  lines.push(`tags = { role = "agent" }`);
  lines.push('');
  lines.push(`[sessions.${opts.sessionName}.env]`);
  lines.push(`ST_AGENT = "${tomlEscape(opts.identity)}"`);
  if (opts.addDingSidecar) {
    const dingLine = `coord ding ${shellQuote(opts.sessionName)} --identity ${shellQuote(opts.identity)}`;
    lines.push('');
    lines.push(`[sessions.ding]`);
    lines.push(`command = "${tomlEscape(dingLine)}"`);
    lines.push(`tags = { role = "ding", strategy = "permanent" }`);
    lines.push('');
    lines.push(`[sessions.ding.env]`);
    lines.push(`ST_AGENT = "${tomlEscape(opts.identity)}"`);
  }
  return lines.join('\n') + '\n';
}

// ─── Claude Code settings.local.json (brief-118) ───────────────────────

/**
 * Resolve the absolute path to the shipped Claude Code hooks directory
 * (`<smalltalk-root>/examples/claude-code/hooks/`). Uses the same
 * package-json walk as {@link resolveCoordBinPath}: we know that path
 * gives `<smalltalk-root>/bin/coord`, so the hooks live at
 * `<smalltalk-root>/examples/claude-code/hooks/`.
 *
 * Returns `null` when either the coord binary path can't be resolved
 * (e.g. degenerate PATH), or the derived hooks directory doesn't
 * exist on disk (e.g. an npm install that didn't ship `examples/`).
 * A null return is a soft skip — the launch still succeeds; the
 * caller just doesn't write `settings.local.json`.
 */
export function resolveClaudeHooksDir(): string | null {
  let coordBin: string;
  try {
    coordBin = resolveCoordBinPath();
  } catch {
    return null;
  }
  const smalltalkRoot = dirname(dirname(coordBin));
  const hooksDir = join(smalltalkRoot, 'examples', 'claude-code', 'hooks');
  try {
    if (statSync(hooksDir).isDirectory()) return hooksDir;
  } catch {
    // hooks dir absent — soft skip.
  }
  return null;
}

/**
 * Format a hook `command:` string for `.claude/settings.local.json`.
 * When `stBin` is non-null, prepends `ST_BIN=<abs>` as a shell
 * assignment so the hook script sees an absolute-path binary regardless
 * of PATH state. Claude Code executes shell-form commands (no `args`)
 * via `sh -c`, so the leading `VAR=value cmd` form is parsed as an
 * assignment-preceded simple command per POSIX.
 *
 * When `stBin` is null (bin/st not found alongside the resolved coord
 * bin — an unusual state), the assignment is omitted and the hook
 * falls back to its own `command -v st || command -v coord` PATH
 * lookup. That keeps the hook usable under hand-wired settings.local
 * files that never went through this generator.
 */
function hookCommand(hookScript: string, stBin: string | null): string {
  const quotedScript = shellQuote(hookScript);
  if (stBin === null) return quotedScript;
  return `ST_BIN=${shellQuote(stBin)} ${quotedScript}`;
}

/** Resolve the absolute path to `bin/st` next to a resolved bin/coord.
 *  Same directory as coordBin; different filename. Returns null when
 *  the sibling isn't present on disk (paranoid — bin/st has shipped
 *  since brief-005-phase0, but not worth crashing settings generation
 *  over an installer glitch). */
export function resolveStBinPath(coordBin: string): string | null {
  const candidate = join(dirname(coordBin), 'st');
  try {
    if (statSync(candidate).isFile()) return candidate;
  } catch {
    // absent
  }
  return null;
}

/**
 * Build the `.claude/settings.local.json` content wiring the three
 * smalltalk hooks:
 *   - **SessionStart** with `async: true` + `asyncRewake: true` so
 *     Claude Code surfaces the hook's stderr as a system reminder
 *     that triggers a turn — closes the "session boots but nothing
 *     wakes the agent to run the boot ritual" gap the polling
 *     backstop (brief-020) partially addressed. asyncRewake IS the
 *     complementary boot leg to the polling backstop.
 *   - **PreCompact** (task #33 hook-legs) — writes a stub to
 *     `context/now.md` if the model hasn't recently flushed, so
 *     boot-rehydrate has something to inject after compaction.
 *   - **StopFailure** — surfaces API-error wedges to myobie via the
 *     st CLI so a quiet, wedged session doesn't go unnoticed.
 *
 * All three point at the shipped scripts under
 * `<smalltalk-root>/examples/claude-code/hooks/` via absolute paths.
 * Claude Code does NOT resolve `~` or repo-relative paths in hook
 * commands — that's why we bake the absolute path here.
 *
 * `stBin` is the absolute path to the `st` CLI. Injected as
 * `ST_BIN=<abs>` in the hook `command:` strings so the hook scripts'
 * shellouts use the same binary the operator launched under —
 * robust to a degenerate PATH or a stale `coord` on PATH. Pass `null`
 * to skip the injection and let the hooks fall back to their internal
 * `command -v st || command -v coord` lookup.
 */
export function buildClaudeSettings(
  hooksDir: string,
  stBin: string | null
): string {
  const settings = {
    $schema: 'https://json.schemastore.org/claude-code-settings.json',
    hooks: {
      SessionStart: [
        {
          hooks: [
            {
              type: 'command',
              async: true,
              asyncRewake: true,
              command: hookCommand(join(hooksDir, 'session-start.sh'), stBin),
            },
          ],
        },
      ],
      PreCompact: [
        {
          hooks: [
            {
              type: 'command',
              command: hookCommand(join(hooksDir, 'pre-compact.sh'), stBin),
            },
          ],
        },
      ],
      StopFailure: [
        {
          hooks: [
            {
              type: 'command',
              command: hookCommand(join(hooksDir, 'stop-failure.sh'), stBin),
            },
          ],
        },
      ],
    },
  };
  return `${JSON.stringify(settings, null, 2)}\n`;
}

// ─── brief-022: persona install ────────────────────────────────────────

/** The infra files `st launch` creates in the target cwd. Persona-mode
 *  always excludes these (except entryFile — see below). Kept as an
 *  exported const so the CLI dry-run summary + tests can enumerate the
 *  same list without duplicating it. */
export const PERSONA_ALWAYS_EXCLUDE = [
  'PERSONA.md',
  '.mcp.json',
  '.claude-session-id',
  '.codex-session-id',
  'pty.toml',
] as const;

/** Read the current `.git/info/exclude`, or empty string if missing.
 *  Used both for the "is this line already there?" dedup and for the
 *  final append. Failures (e.g. `.git` isn't a dir) surface as empty. */
function readGitExclude(cwd: string): { path: string; text: string } | null {
  const gitDir = join(cwd, '.git');
  // A bare git repo, worktree, or non-git directory won't have
  // .git/info/. Skip cleanly; callers surface `gitRepoAbsent: true`.
  try {
    const st = statSync(gitDir);
    if (!st.isDirectory()) return null;
  } catch {
    return null;
  }
  const excludePath = join(gitDir, 'info', 'exclude');
  let text = '';
  try {
    text = readFileSync(excludePath, 'utf8');
  } catch {
    // missing is fine; append semantics create it
  }
  return { path: excludePath, text };
}

/**
 * Return the entries from `wanted` that aren't already present as a
 * line in the exclude file. Comparison is line-by-line, trimmed.
 * Comments (leading `#`) are ignored so `# PERSONA.md example` doesn't
 * count as a match.
 */
function missingExcludeEntries(
  excludeText: string,
  wanted: readonly string[]
): string[] {
  const present = new Set<string>();
  for (const rawLine of excludeText.split(/\r?\n/)) {
    const trimmed = rawLine.trim();
    if (trimmed === '' || trimmed.startsWith('#')) continue;
    present.add(trimmed);
  }
  return wanted.filter((w) => !present.has(w));
}

/**
 * Append `entries` (one per line) to `.git/info/exclude`. Ensures a
 * trailing newline separates existing content from the append. Creates
 * the file (and its parent `info/` dir) if needed. Idempotent when
 * called with an empty entries list.
 */
function appendGitExclude(
  excludePath: string,
  existingText: string,
  entries: readonly string[]
): void {
  if (entries.length === 0) return;
  mkdirSync(dirname(excludePath), { recursive: true });
  const sep = existingText.length === 0 || existingText.endsWith('\n')
    ? ''
    : '\n';
  const blockHeader =
    existingText.length === 0 ? '' : '# smalltalk-launch persona infra\n';
  const appended = `${sep}${blockHeader}${entries.join('\n')}\n`;
  writeFileSync(excludePath, existingText + appended);
}

/**
 * Decide whether the entry file (`CLAUDE.md` / `AGENTS.md`) needs the
 * `@PERSONA.md` import line appended. Returns a struct with the read
 * text (empty when the file didn't exist), whether we'd create the
 * file, and whether we'd append the import.
 */
function planEntryFileEdit(entryFilePath: string): {
  existingText: string;
  fileExisted: boolean;
  importAlreadyPresent: boolean;
} {
  let existingText = '';
  let fileExisted = false;
  try {
    existingText = readFileSync(entryFilePath, 'utf8');
    fileExisted = true;
  } catch {
    // missing file → we'll create it
  }
  // The import line is a bare `@PERSONA.md` on its own line. We match
  // leniently so `@PERSONA.md ` (trailing space) or `@PERSONA.md #
  // comment` are recognized as already-present. Case-sensitive on
  // purpose — the filename is `PERSONA.md`.
  const importPattern = /^@PERSONA\.md\b/m;
  const importAlreadyPresent = importPattern.test(existingText);
  return { existingText, fileExisted, importAlreadyPresent };
}

/**
 * Do (or plan) the persona install. When `dryRun` is true, returns
 * the plan without touching disk; when false, performs the copy /
 * entry-file edit / git-exclude append. All decisions (including
 * `gitExcludeEntriesAdded`) reflect what actually happens, so the
 * dry-run summary matches a real run byte-for-byte.
 */
function installPersona(opts: {
  personaSourcePath: string;
  cwd: string;
  harness: Harness;
  dryRun: boolean;
}): PersonaInstallResult {
  const { personaSourcePath, cwd, harness, dryRun } = opts;
  const entryFile: PersonaInstallResult['entryFile'] =
    harness === 'claude' ? 'CLAUDE.md' : 'AGENTS.md';
  const entryFilePath = join(cwd, entryFile);
  const personaMdPath = join(cwd, 'PERSONA.md');

  // Read the source persona now so a bad --persona path fails loudly
  // (missing file, unreadable, etc.) before we've touched anything on
  // disk. Content is used both to write PERSONA.md in real-run and to
  // guarantee the source is readable in dry-run.
  const personaText = readFileSync(personaSourcePath, 'utf8');

  const entryPlan = planEntryFileEdit(entryFilePath);
  const importLineToAppend = !entryPlan.importAlreadyPresent;
  const entryFileCreated =
    !entryPlan.fileExisted && importLineToAppend;

  // Compute git-exclude entries. Always exclude PERSONA_ALWAYS_EXCLUDE
  // (only those not already present). Additionally exclude the
  // entryFile if we created it — never exclude a pre-existing repo
  // CLAUDE.md.
  const wantedExcludes: string[] = [...PERSONA_ALWAYS_EXCLUDE];
  if (entryFileCreated) wantedExcludes.push(entryFile);

  const excludeInfo = readGitExclude(cwd);
  const gitRepoAbsent = excludeInfo === null;
  const gitExcludeEntriesAdded = excludeInfo
    ? missingExcludeEntries(excludeInfo.text, wantedExcludes)
    : [];

  if (!dryRun) {
    // Copy the persona source → PERSONA.md. If the file already
    // exists in cwd with different content, we OVERWRITE — this is
    // infra we own, not user content (we git-exclude it), and the
    // caller is telling us the source is authoritative for this
    // launch. copyFileSync preserves nothing about the source's
    // permissions we don't want; markdown is markdown.
    copyFileSync(personaSourcePath, personaMdPath);

    // Entry-file edit. If missing → create with just the import line
    // (+ trailing newline). If present without the import → append
    // "\n@PERSONA.md\n" with a leading blank-line separator when the
    // existing content doesn't already end in a newline.
    if (importLineToAppend) {
      if (!entryPlan.fileExisted) {
        writeFileSync(entryFilePath, '@PERSONA.md\n');
      } else {
        const sep =
          entryPlan.existingText.length === 0 ||
          entryPlan.existingText.endsWith('\n')
            ? ''
            : '\n';
        writeFileSync(
          entryFilePath,
          entryPlan.existingText + sep + '@PERSONA.md\n'
        );
      }
    }

    // Git-exclude append (skips silently when not a git repo).
    if (excludeInfo && gitExcludeEntriesAdded.length > 0) {
      appendGitExclude(
        excludeInfo.path,
        excludeInfo.text,
        gitExcludeEntriesAdded
      );
    }
  }

  // Reference `personaText` so the read isn't optimized away in a
  // future refactor — the read serves as an eager error surface for
  // bad --persona paths, so we want the read to happen before any
  // downstream work does.
  void personaText;

  return {
    personaMdPath,
    entryFile,
    entryFilePath,
    entryFileCreated,
    importLineAppended: importLineToAppend,
    gitExcludeEntriesAdded,
    gitRepoAbsent,
  };
}

// ─── Core ──────────────────────────────────────────────────────────────

export async function cmdLaunch(
  input: LaunchInput,
  ctx: CliContext
): Promise<LaunchResult> {
  const cwd = input.cwd ?? process.cwd();
  const home = input.home ?? homedir();
  const harness = input.harness;

  // ─── Identity resolution ────────────────────────────────────────────
  let identity: string;
  let identityAutoGenerated = false;
  if (input.identity !== undefined && input.identity.length > 0) {
    identity = input.identity;
  } else {
    const fromEnv = envAgentFrom(input.env);
    if (fromEnv !== undefined && fromEnv.length > 0) {
      identity = fromEnv;
    } else {
      identity = generateAnonAgent();
      identityAutoGenerated = true;
      ctx.stderr(
        `[smalltalk] no ST_AGENT set; using throwaway identity ${identity} ` +
          `(set ST_AGENT to persist)\n`
      );
    }
  }
  // Lazy-create the agent's inbox/archive so channel watcher + status
  // writer have something to point at.
  if (!input.dryRun) {
    ensureIdentityDirs(identity, input.coordRoot);
  }

  // ─── .mcp.json bootstrap (via cmdInit) ──────────────────────────────
  const channel =
    input.noChannel === true ? false : harness === 'claude';
  const mcpJsonPath = join(cwd, '.mcp.json');
  if (!input.dryRun) {
    await cmdInit(
      {
        dir: cwd,
        noChannel: !channel,
        force: false,
        // promptAnswer: undefined — leave the existing prompt-gate
        // behavior; a divergent .mcp.json aborts rather than silently
        // overwriting.
      },
      ctx
    );
  }

  // ─── brief-022: persona install ─────────────────────────────────────
  // Runs BEFORE session-id / command construction so the entry file
  // (CLAUDE.md/AGENTS.md) is in place before the harness process
  // reads it. Both dry-run and real-run go through `installPersona` —
  // the returned plan is what we display in --dry-run and what we
  // actually did in real-run; only the file I/O differs internally.
  let personaResult: PersonaInstallResult | null = null;
  if (input.persona !== undefined && input.persona.length > 0) {
    const source = isAbsolute(input.persona)
      ? input.persona
      : resolve(process.cwd(), input.persona);
    try {
      personaResult = installPersona({
        personaSourcePath: source,
        cwd,
        harness,
        dryRun: input.dryRun === true,
      });
    } catch (err) {
      // Surface a clean error that names the flag and the source
      // path rather than the raw ENOENT — the caller almost always
      // wants to know "where did you look?"
      const msg = err instanceof Error ? err.message : String(err);
      throw new Error(
        `--persona: could not read persona file at ${source}: ${msg}`
      );
    }
    if (personaResult.gitRepoAbsent) {
      ctx.stderr(
        `[smalltalk launch] --persona: ${cwd} is not a git repo; ` +
          `skipping .git/info/exclude update. The persona files are ` +
          `still installed.\n`
      );
    }
  }

  // ─── claude session-id bootstrap ────────────────────────────────────
  let claudeSessionIdPath: string | null = null;
  let claudeSessionId: string | null = null;
  if (harness === 'claude') {
    const idFile = join(cwd, '.claude-session-id');
    claudeSessionIdPath = idFile;
    if (existsSync(idFile)) {
      claudeSessionId = readFileSync(idFile, 'utf8').trim();
    }
    if (
      (claudeSessionId === null || claudeSessionId.length === 0) &&
      !input.dryRun
    ) {
      claudeSessionId = newUuid();
      writeFileSync(idFile, `${claudeSessionId}\n`);
    } else if (claudeSessionId === null) {
      claudeSessionId = '<generated-at-runtime>';
    }
  }

  // ─── brief-023: permission-mode resolution ─────────────────────────
  // Precedence: explicit --permission-mode flag > $CLAUDE_PERMISSION_MODE
  // env (parity with pty-claude-launcher.sh's fallback) > default 'auto'.
  // 'auto' preserves the pre-brief-023 behavior byte-for-byte, so
  // existing callers see no change. Value is passed through to claude
  // verbatim — we don't validate here (claude rejects unknown modes
  // loudly, and duplicating the enum would just rot).
  const permissionMode: string =
    input.permissionMode !== undefined && input.permissionMode.length > 0
      ? input.permissionMode
      : input.env.CLAUDE_PERMISSION_MODE !== undefined &&
          input.env.CLAUDE_PERMISSION_MODE.length > 0
        ? input.env.CLAUDE_PERMISSION_MODE
        : 'auto';

  // ─── agent-binary resolution (task: Johannes's cl1/cl2 aliases) ─────
  // Precedence: explicit `--agent` flag > `$AGENT` env > `'claude'`
  // default. Only affects the `claude` harness; codex is untouched (it
  // has its own launcher path). Preserves the pre-task behavior
  // byte-for-byte when neither flag nor env is set.
  const agentBinary: string =
    input.agentBinary !== undefined && input.agentBinary.length > 0
      ? input.agentBinary
      : input.env.AGENT !== undefined && input.env.AGENT.length > 0
        ? input.env.AGENT
        : 'claude';

  // ─── Command construction ──────────────────────────────────────────
  let argv: readonly string[];
  let usedOllama = false;
  let claudeBootstrapArgv: readonly string[] | null = null;
  let claudeJsonlPath: string | null = null;
  if (input.model !== undefined && input.model.length > 0) {
    // Route through ollama — its `--model` flag skips the interactive
    // picker AND injects env vars claude/codex need to hit the
    // ollama-hosted model.
    argv = buildOllamaCommand(harness, input.model);
    usedOllama = true;
  } else if (harness === 'claude') {
    const built = buildClaudeCommand({
      cwd,
      home,
      channel,
      claudeSessionId: claudeSessionId ?? '<generated-at-runtime>',
      permissionMode,
      agentBinary,
    });
    argv = built.mainArgv;
    claudeBootstrapArgv = built.bootstrapArgv;
    claudeJsonlPath = built.jsonlPath;
  } else {
    argv = buildCodexCommand(cwd);
  }

  // ─── pty registration decision ─────────────────────────────────────
  const ptyPath = input.noPty === true ? null : detectPtyPath(input.ptyBinPath);
  const usedPty = ptyPath !== null;
  const sessionName = input.sessionName ?? harness;
  const ptyTomlPath = join(cwd, 'pty.toml');
  let ptyTomlPreview: string | null = null;
  if (usedPty) {
    const preview = buildPtyToml({
      harness,
      sessionName,
      identity,
      invocation: argv,
      addDingSidecar: harness === 'codex',
    });
    ptyTomlPreview = preview;
    if (!input.dryRun && !existsSync(ptyTomlPath)) {
      writeFileSync(ptyTomlPath, preview);
    }
    // else: skip-if-exists. User can `rm pty.toml` to re-bootstrap.
  } else {
    // Emit a suggested pty.toml even when pty isn't present so the
    // user can install pty later and drop it in.
    ptyTomlPreview = buildPtyToml({
      harness,
      sessionName,
      identity,
      invocation: argv,
      addDingSidecar: harness === 'codex',
    });
  }

  // ─── Claude settings.local.json (brief-118 asyncRewake wiring) ─────
  // Only for the claude harness, and only when `--no-hooks` wasn't
  // passed. Skip-if-exists convention: if `.claude/settings.local.json`
  // already exists we leave it alone — user may have hand-tuned it and
  // silently overwriting would erase their customizations. Under
  // --dry-run we still populate the preview so the summary can show
  // what would have been written.
  let claudeSettingsPath: string | null = null;
  let claudeSettingsPreview: string | null = null;
  if (harness === 'claude' && input.noHooks !== true) {
    let hooksDir: string | null;
    if (input.hooksDir !== undefined) {
      // Explicit override — verify the path still exists so a stale
      // seam value produces the same soft-skip as the auto-resolution
      // failure path. Prevents baking a bad absolute path into the
      // generated JSON.
      hooksDir = existsSync(input.hooksDir) ? input.hooksDir : null;
    } else {
      hooksDir = resolveClaudeHooksDir();
    }
    if (hooksDir !== null) {
      const settingsDir = join(cwd, '.claude');
      const settingsFile = join(settingsDir, 'settings.local.json');
      claudeSettingsPath = settingsFile;
      // Resolve the absolute `st` binary path for ST_BIN injection into
      // hook `command:` strings. Same package.json walk as the coord
      // path (they're siblings in bin/). Null on resolution failure
      // (degenerate PATH + tree not on disk); the hook scripts have
      // their own PATH fallback so the launch still succeeds. Test
      // seam: `input.stBinForHooks` overrides — `null` opts out of
      // injection entirely, a string overrides the auto-resolved
      // path (for deterministic snapshots).
      let stBinForHooks: string | null;
      if (input.stBinForHooks !== undefined) {
        stBinForHooks = input.stBinForHooks;
      } else {
        try {
          stBinForHooks = resolveStBinPath(resolveCoordBinPath());
        } catch {
          stBinForHooks = null;
        }
      }
      claudeSettingsPreview = buildClaudeSettings(hooksDir, stBinForHooks);
      if (!input.dryRun && !existsSync(settingsFile)) {
        mkdirSync(settingsDir, { recursive: true });
        writeFileSync(settingsFile, claudeSettingsPreview);
      }
      // else: skip-if-exists. User can `rm .claude/settings.local.json`
      // to re-bootstrap, or edit it in place if they only want to
      // tweak the hook list.
    } else {
      // Hooks dir absent (npm install without `examples/`, or degenerate
      // PATH). Soft skip: launch still works, agent just doesn't get
      // the boot hooks — same as pre-brief-118 behavior. Emit a hint
      // so the operator can wire hooks by hand if they want.
      ctx.stderr(
        `[smalltalk launch] shipped Claude Code hooks not found on disk; ` +
          `skipping .claude/settings.local.json (see examples/claude-code/settings.local.example.json)\n`
      );
    }
  }

  // ─── Dry-run / captureOnly short-circuit ───────────────────────────
  if (input.dryRun === true || input.captureOnly === true) {
    return {
      identity,
      identityAutoGenerated,
      channel,
      usedPty,
      usedOllama,
      argv,
      mcpJsonPath,
      ptyTomlPath: usedPty ? ptyTomlPath : null,
      claudeSessionIdPath,
      ptyTomlPreview,
      permissionMode,
      agentBinary,
      claudeSettingsPath,
      claudeSettingsPreview,
      persona: personaResult,
    };
  }

  // ─── Claude bootstrap (jsonl init) ─────────────────────────────────
  // Only fires when the jsonl is missing AND we're going through the
  // direct claude path (not ollama).
  if (
    harness === 'claude' &&
    !usedOllama &&
    claudeBootstrapArgv !== null &&
    claudeJsonlPath !== null &&
    !existsSync(claudeJsonlPath)
  ) {
    ctx.stderr(
      `[smalltalk launch] bootstrapping claude jsonl at ${claudeJsonlPath}\n`
    );
    spawnSync(claudeBootstrapArgv[0]!, claudeBootstrapArgv.slice(1), {
      stdio: ['ignore', 'ignore', 'ignore'],
      env: {
        ...input.env,
        ST_AGENT: identity,
      },
    });
    // Ignore result — the bootstrap is best-effort. If it failed, the
    // main --resume will surface the error next.
  }

  // ─── Spawn ─────────────────────────────────────────────────────────
  const spawnEnv = {
    ...input.env,
    ST_AGENT: identity,
  };
  if (usedPty && ptyPath !== null) {
    // pty picks the sessions up from pty.toml. Exec pty up + wait.
    const child = spawn(ptyPath, ['up'], {
      stdio: 'inherit',
      env: spawnEnv,
      cwd,
    });
    await new Promise<void>((resolvePromise) => {
      child.on('exit', () => resolvePromise());
    });
  } else {
    // Direct spawn of the harness. `stdio: inherit` gives the user
    // the terminal.
    const child = spawn(argv[0]!, argv.slice(1), {
      stdio: 'inherit',
      env: spawnEnv,
      cwd,
    });
    await new Promise<void>((resolvePromise) => {
      child.on('exit', () => resolvePromise());
    });
  }

  return {
    identity,
    identityAutoGenerated,
    channel,
    usedPty,
    usedOllama,
    argv,
    mcpJsonPath,
    ptyTomlPath: usedPty ? ptyTomlPath : null,
    claudeSessionIdPath,
    ptyTomlPreview,
    permissionMode,
    agentBinary,
    claudeSettingsPath,
    claudeSettingsPreview,
    persona: personaResult,
  };
}

// ─── CLI wrapper ────────────────────────────────────────────────────────

const LAUNCH_HELP =
  'usage: st launch <claude|codex> [options]\n\n' +
  '  Stand up an agent wired to smalltalk in one command. Sets up\n' +
  '  identity, .mcp.json, session-id (claude only), and optionally\n' +
  '  registers the session via pty when it is on PATH.\n\n' +
  '  --identity <name>       Explicit agent name. Else $ST_AGENT (with\n' +
  '                          legacy ST_IDENTITY / COORD_IDENTITY chain),\n' +
  '                          else a throwaway anon-<rand6>.\n' +
  '  --model <spec>          Route through `ollama launch <harness>\n' +
  '                          --model <spec>` — skips ollama\'s interactive\n' +
  '                          model picker, unblocks unattended GLM launches.\n' +
  '  --no-channel            Skip the --channel MCP wiring. Default for\n' +
  '                          claude is channel-on; for codex is channel-off.\n' +
  '  --no-pty                Don\'t register via pty even if it is on PATH.\n' +
  '  --no-hooks              Don\'t generate .claude/settings.local.json (the\n' +
  '                          SessionStart+asyncRewake / PreCompact / StopFailure\n' +
  '                          boot hooks). Claude launches opt in by default.\n' +
  '  --session-name <name>   Override pty session key. Default: harness name.\n' +
  '  --permission-mode <mode>\n' +
  '                          Claude `--permission-mode` value. Threaded into\n' +
  '                          the harness argv AND the generated pty.toml.\n' +
  '                          Values pass through to claude verbatim:\n' +
  '                          acceptEdits, auto, bypassPermissions, default,\n' +
  '                          dontAsk, plan. Precedence: this flag >\n' +
  '                          $CLAUDE_PERMISSION_MODE > default `auto`. Codex\n' +
  '                          launches ignore this (its own surface).\n' +
  '  --persona <path>        Install <path> as PERSONA.md and surgically\n' +
  '                          add `@PERSONA.md` to CLAUDE.md (claude) or\n' +
  '                          AGENTS.md (codex). Pre-existing entry files\n' +
  '                          are NEVER clobbered — only the import line\n' +
  '                          is appended if not already present. Infra we\n' +
  '                          create is added to `.git/info/exclude` so it\n' +
  '                          stays out of the repo.\n' +
  '  --agent <name>          Invoke <name> as the harness binary instead\n' +
  '                          of `claude`. For hosts running aliased builds\n' +
  '                          (e.g. `cl1`, `cl2`, `claude-preview`).\n' +
  '                          Precedence: this flag > $AGENT env > `claude`.\n' +
  '                          Codex launches ignore this (its own launcher).\n' +
  '  --dry-run               Print what would happen; touch nothing.\n' +
  '  --print                 Alias for --dry-run.\n\n' +
  '  Examples:\n' +
  '    st launch claude                              # anon identity, channel mode + boot hooks\n' +
  '    st launch claude --identity alice             # persistent identity\n' +
  '    st launch claude --no-hooks                   # skip .claude/settings.local.json\n' +
  '    st launch codex                               # + coord ding sidecar\n' +
  '    st launch claude --model glm-5.2:cloud        # via ollama, unattended\n' +
  '    st launch claude --permission-mode bypassPermissions   # eval-spinner posture\n' +
  '    st launch codex --dry-run                     # audit before spawn\n';

export async function cmdLaunchCli(
  args: readonly string[],
  ctx: CliContext
): Promise<number> {
  let harness: Harness | undefined;
  let identity: string | undefined;
  let model: string | undefined;
  let noPty = false;
  let noChannel = false;
  let noHooks = false;
  let sessionName: string | undefined;
  let permissionMode: string | undefined;
  let persona: string | undefined;
  let agentBinary: string | undefined;
  let dryRun = false;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '-h':
      case '--help':
        ctx.stderr(LAUNCH_HELP);
        return 0;
      case '--identity':
        identity = args[++i];
        break;
      case '--model':
        model = args[++i];
        break;
      case '--no-pty':
        noPty = true;
        break;
      case '--no-channel':
        noChannel = true;
        break;
      case '--no-hooks':
        noHooks = true;
        break;
      case '--session-name':
        sessionName = args[++i];
        break;
      case '--permission-mode':
        permissionMode = args[++i];
        break;
      case '--persona':
        persona = args[++i];
        break;
      case '--agent':
        agentBinary = args[++i];
        break;
      case '--dry-run':
      case '--print':
        dryRun = true;
        break;
      default:
        if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
        if (harness === undefined) {
          if (a !== 'claude' && a !== 'codex') {
            throw new Error(
              `unknown harness: ${a} (supported: claude, codex)`
            );
          }
          harness = a;
        } else {
          throw new Error(`unexpected arg: ${a}`);
        }
    }
  }
  if (harness === undefined) {
    ctx.stderr(LAUNCH_HELP);
    return 2;
  }

  const r = await cmdLaunch(
    {
      harness,
      ...(identity !== undefined && { identity }),
      ...(model !== undefined && { model }),
      noPty,
      noChannel,
      noHooks,
      ...(sessionName !== undefined && { sessionName }),
      ...(permissionMode !== undefined && { permissionMode }),
      ...(persona !== undefined && { persona }),
      ...(agentBinary !== undefined && { agentBinary }),
      dryRun,
      env: ctx.env,
      coordRoot: ctx.coordRoot,
    },
    ctx
  );

  if (dryRun) {
    // Human-readable summary for --dry-run.
    ctx.stdout(`identity:       ${r.identity}\n`);
    if (r.identityAutoGenerated) {
      ctx.stdout(`                (throwaway; set ST_AGENT to persist)\n`);
    }
    ctx.stdout(`channel mode:   ${r.channel ? 'on' : 'off'}\n`);
    ctx.stdout(`ollama route:   ${r.usedOllama ? 'yes' : 'no'}\n`);
    ctx.stdout(`pty available:  ${r.usedPty ? 'yes' : 'no'}\n`);
    // brief-023: show the resolved --permission-mode. Claude-only surface
    // effect, but we surface it always so `codex --dry-run` audits still
    // reveal what the resolution logic decided (helps debugging env-set
    // launches).
    ctx.stdout(`permission-mode: ${r.permissionMode}\n`);
    ctx.stdout(`agent binary:   ${r.agentBinary}\n`);
    ctx.stdout(`mcp.json:       ${r.mcpJsonPath}\n`);
    if (r.claudeSessionIdPath !== null) {
      ctx.stdout(`session id:     ${r.claudeSessionIdPath}\n`);
    }
    if (r.claudeSettingsPath !== null) {
      ctx.stdout(`claude hooks:   ${r.claudeSettingsPath}\n`);
    }
    ctx.stdout(`\nharness argv:\n  ${r.argv.join(' ')}\n`);
    if (r.ptyTomlPreview !== null) {
      const heading = r.usedPty
        ? `\npty.toml (would write to ${r.ptyTomlPath}):`
        : `\npty.toml (pty not on PATH; write this file by hand if you want it):`;
      ctx.stdout(`${heading}\n${r.ptyTomlPreview}`);
    }
    if (r.claudeSettingsPreview !== null && r.claudeSettingsPath !== null) {
      ctx.stdout(
        `\n.claude/settings.local.json (would write to ${r.claudeSettingsPath}):\n${r.claudeSettingsPreview}`
      );
    }
    if (r.persona !== null) {
      // Persona summary — reflect the exact decisions installPersona
      // made, so the operator can eyeball "did we create the entry
      // file? did we edit an existing one? which lines are we adding
      // to git-exclude?" without running for real first.
      ctx.stdout(`\npersona:\n`);
      ctx.stdout(`  copy:         ${r.persona.personaMdPath}\n`);
      const action = r.persona.entryFileCreated
        ? 'create'
        : r.persona.importLineAppended
          ? 'append @PERSONA.md'
          : 'no change (import already present)';
      ctx.stdout(`  entry file:   ${r.persona.entryFilePath} (${action})\n`);
      if (r.persona.gitRepoAbsent) {
        ctx.stdout(
          `  git-exclude:  skipped (${cwdForGitExcludeNote(r)} is not a git repo)\n`
        );
      } else if (r.persona.gitExcludeEntriesAdded.length === 0) {
        ctx.stdout(`  git-exclude:  no new entries (all already present)\n`);
      } else {
        ctx.stdout(
          `  git-exclude:  ${r.persona.gitExcludeEntriesAdded.join(', ')}\n`
        );
      }
    }
    return 0;
  }
  return 0;
}

/** Small helper: extract the target cwd from the launch result path
 *  so we don't need to pass it through separately. All the fields we
 *  care about live under the same cwd, and dirname of the entryFile
 *  is the cleanest source. */
function cwdForGitExcludeNote(r: {
  persona: PersonaInstallResult | null;
}): string {
  return r.persona ? dirname(r.persona.entryFilePath) : '.';
}

// Keep resolveCoordBinPath reachable for `notes/harness-integrations.md`
// linkage; also used by the tests that mock cmdInit.
export { resolveCoordBinPath };
