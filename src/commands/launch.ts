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
//     to non-channel + an `st ding` sidecar (no asyncRewake).

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
  /**
   * Ding-mode: launch claude the way codex launches — WITHOUT MCP
   * wiring, WITH an `st ding` sidecar for inbox delivery. Skips
   * `.mcp.json` generation entirely (no `cmdInit` call) and forces
   * `channel = false` (no `--dangerously-load-development-channels`
   * flag on the argv). Adds the ding sidecar to the generated
   * pty.toml so the claude agent gets inbox notifications via
   * `pty send` instead of MCP `notifications/claude/channel`.
   *
   * The load-bearing use case: environments where MCP servers don't
   * work at all (Johannes's setup, some sandboxes). Without ding-mode,
   * `st launch claude` requires a functioning MCP transport; with
   * ding-mode, the same agent joins the network via ding delivery +
   * the `st` CLI for all bus ops (send, ls, read, archive, reply).
   *
   * Hooks (`.claude/settings.local.json`) are still generated — the
   * boot ritual + PreCompact flush + StopFailure ding are Claude
   * Code hooks, independent of MCP.
   *
   * No-op for codex (codex is already ding-mode by default —
   * `addDingSidecar` fires unconditionally for that harness).
   */
  ding?: boolean | undefined;
  /** Override the pty session name (default: harness name). */
  sessionName?: string | undefined;
  /**
   * When true, prepend a startup auto-poker to the pty session
   * command so Claude Code's first-launch TUI gates (workspace trust,
   * `--dangerously-load-development-channels` warning, optional
   * resume-mode dialog) get an Enter each without a human at the
   * REPL. The poker `pty send`s Enter 4 times with 4s spacing, so
   * extra pokes past the last real dialog just fire empty prompts —
   * no-ops. Only applies when both `harness === 'claude'` and the
   * pty registration succeeds. Ignored for codex (no dev-channels
   * gate) and for `--no-pty` (nothing to send to).
   *
   * Resolution at the CLI level: `--unattended` flag wins; when
   * unset, auto-on when `stdinIsTty` reports non-TTY (a CoS spawning
   * a specialist via `spawn` gets this for free with no flag).
   */
  unattended?: boolean | undefined;
  /**
   * When true, mark BOTH the agent session AND (for codex) the ding
   * sidecar with `strategy = "permanent"` in the generated pty.toml
   * so pty resurrects them if their daemons die and `pty gc`
   * doesn't reap them under idle-cleanup. Load-bearing for a
   * production CoS (or any always-on agent) that must survive
   * across restarts and cleanup passes.
   *
   * When false / unset (default), the launch is ephemeral: agent
   * and ding both carry no `strategy` tag, and pty treats them as
   * its ephemeral default (see `../pty/src/sessions.ts:576, 620,
   * 644`). Matches the historic bare-agent shape + the ding fix
   * that just landed in the previous PR.
   *
   * A CoS-shaped launch that OMITS this flag (via `--identity cos`
   * or a chief-of-staff persona) triggers a stderr warning
   * ("launching a CoS without --permanent; pty gc may reap it"),
   * because a silently-reap-able CoS is a nasty non-obvious
   * failure for a newcomer following the onboarding docs.
   */
  permanent?: boolean | undefined;
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
   * True when the auto-poker got baked into the pty session command.
   * Populated regardless of whether pty was actually resolved (a
   * captureOnly / dry-run launch reports it consistently) so callers
   * can log whether hands-off standup was in force.
   */
  unattended: boolean;
  /**
   * True when the launch was marked permanent (--permanent flag).
   * Both the agent session AND the ding sidecar (codex) got
   * `strategy = "permanent"` in the generated pty.toml. False for
   * the default ephemeral launch. Populated regardless of pty
   * availability so dry-run summaries surface the resolution
   * consistently.
   */
  permanent: boolean;
  /**
   * True when the launch ran in ding-mode: MCP wiring skipped
   * (no `.mcp.json` generated) and an `st ding` sidecar added to
   * pty.toml regardless of harness. Populated in both real-run and
   * dry-run branches so the summary can surface the mode.
   */
  ding: boolean;
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
  /**
   * Summary of the DING-BUS.md install, or null when the launch
   * wasn't `--ding` (or was codex, which has its own instructions
   * path). See {@link installDingBusInstructions} + the
   * {@link DING_BUS_INSTRUCTIONS} constant for content.
   */
  dingBus: DingBusInstallResult | null;
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

/**
 * Result of the DING-BUS.md install (only runs on `--ding` claude
 * launches). Same shape as PersonaInstallResult but for the
 * `@DING-BUS.md` import — see {@link installDingBusInstructions}.
 */
export interface DingBusInstallResult {
  /** Absolute path we copied DING-BUS.md to. */
  dingBusMdPath: string;
  /** The entry file (CLAUDE.md — ding-mode is claude-only). */
  entryFile: 'CLAUDE.md';
  /** Absolute path of the entry file. */
  entryFilePath: string;
  /** True when the entry file didn't exist and we created it. */
  entryFileCreated: boolean;
  /** True when we appended a `@DING-BUS.md` line (vs. it was already
   *  present). */
  importLineAppended: boolean;
  /** Entries that were (or would be) appended to `.git/info/exclude`. */
  gitExcludeEntriesAdded: readonly string[];
  /** True when the target cwd was not a git repo, so we couldn't
   *  exclude. Callers can surface a warning; the DING-BUS.md still
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

/**
 * Detect a "spawner-shaped" launch — an agent whose role is to
 * spawn other agents (Nathan's 3-tier hierarchy: cos + supervisor
 * are spawners; workers are leaves). Detection:
 *   - identity ∈ {cos, supervisor}, OR
 *   - persona basename ∈ {chief-of-staff.md, supervisor.md}.
 *
 * Two behavior triggers gate on this:
 *   1. Default permission-mode flips from `auto` to
 *      `bypassPermissions` (a spawner in auto is hard-blocked by
 *      claude's auto-mode classifier from creating autonomous
 *      agents — the regression Johannes's pty.toml surfaced).
 *   2. Footgun-guard warning when `--permanent` is omitted (from
 *      the PR-that-added-`--permanent`). Ephemeral eval spawners
 *      intentionally decline permanent; the warning is opt-in
 *      acknowledgment, not a hard block.
 *
 * Workers (identity ∉ spawner list AND persona basename ∉ spawner
 * list) stay on `auto` — correct + safe for a leaf agent that does
 * work but doesn't spawn. This is deliberate asymmetry: default
 * permission-mode auto→bypass is safe (spawners need it, evals
 * want it) but default permanent stays opt-in (evals launch
 * ephemeral supervisors that MUST be reap-able for teardown).
 */
function isSpawnerShaped(
  identity: string,
  persona: string | undefined
): boolean {
  const basename =
    persona !== undefined
      ? persona.split('/').pop()?.split('\\').pop() ?? ''
      : '';
  return (
    identity === 'cos' ||
    identity === 'supervisor' ||
    basename === 'chief-of-staff.md' ||
    basename === 'supervisor.md'
  );
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

/**
 * Startup auto-poker for unattended claude launches. Wraps a shell
 * command with a background subshell that `pty send`s Enter 4 times
 * with 4s spacing, then `exec`s the main command. The pokes clear
 * Claude Code's first-launch TUI gates in order (workspace trust,
 * dev-channels warning, optional resume-mode dialog); extras past
 * the last gate hit the model prompt as empty submissions — no-ops.
 *
 * Target is the pty session name that `ptyfile.ts:58` derives from
 * a `prefix = "…"` line plus a `[sessions.<name>]` block:
 * `${prefix}-${sessionName}` (dash join, NOT slash). We land in
 * `<identity>-<sessionName>` because F3 makes the prefix the
 * identity. Prior to this fix the poker addressed the session with
 * a slash — pty responded with `Session "<x>/<y>" not found`, every
 * poke missed, and the CoS-spawned worker deadlocked at the
 * dev-channels gate. See `../pty/src/ptyfile.ts:58` for the join
 * canonicalization.
 *
 * The `pty` binary is expected to be on PATH — it always is when
 * pty spawned the session in the first place. If pty fell off PATH
 * mid-session, the pokes emit stderr errors and the shell keeps
 * going; the main claude exec is unaffected.
 */
function pokerPrefix(target: string): string {
  const quoted = shellQuote(target);
  const oneShot = `sleep 4 && pty send ${quoted} --seq key:return`;
  return `(${oneShot}; ${oneShot}; ${oneShot}; ${oneShot}) &`;
}

function buildPtyToml(opts: {
  harness: Harness;
  sessionName: string;
  identity: string;
  invocation: readonly string[];
  addDingSidecar: boolean;
  unattended: boolean;
  /**
   * When the invoker had `ST_ROOT` (or legacy `COORD_ROOT`) explicitly
   * set, its resolved absolute value baked into `[sessions.*.env]` as
   * `ST_ROOT`. Undefined means the invoker was on the default state
   * root — nothing baked, so future `pty up`/`pty restart` invocations
   * from any shell inherit whatever ST_ROOT resolution runs then
   * (usually the same default). Only set when the invoker was doing
   * something explicit — pinning an isolation root through restarts.
   */
  stRoot: string | undefined;
  /**
   * Optional `strategy` value baked into BOTH the agent session
   * AND the ding sidecar tags. When undefined (today's only
   * caller), neither session carries a `strategy` tag — pty
   * treats absence-of-tag as its ephemeral default (see
   * `../pty/src/sessions.ts:576, 620, 644` — the ONLY explicit
   * check pty makes is `strategy === "permanent"`). This mirroring
   * is the invariant: a launch is either ephemeral or it isn't;
   * a mismatch (e.g. permanent ding under an ephemeral agent, or
   * vice-versa) is a bug in whatever code sets this. Future
   * flags (e.g. `--permanent` for a production CoS that should
   * survive `pty gc`) will plumb through this single knob.
   *
   * Historically the ding was hardcoded to `strategy = "permanent"`
   * while the agent had no tag — a mismatch that meant an
   * ephemeral codex eval left the ding as a zombie after `pty gc`
   * reaped the codex session. Fixed by making the field a
   * matched pair.
   */
  agentStrategy: string | undefined;
  /**
   * Phase-1 pty isolation label baked into BOTH the agent session
   * AND the ding sidecar tags as `"st.network" = "<value>"`.
   * Always emitted (unlike `stRoot`, which conditionally emits an
   * `ST_ROOT` env var); the goal is a uniform inspection signal
   * that separates "this is a smalltalk-network session" from
   * an operator's ad-hoc pty use. Value is the resolved network
   * root — the same `input.coordRoot` cmdLaunch already computes
   * via `coordRootFrom(ctx.env)`, so it's always a valid path
   * regardless of whether the invoker set `ST_ROOT` explicitly.
   *
   * pty needs zero change to filter on this: its `--filter-tag
   * st.network=<value>` primitive reads `sessionTags["st.network"]`
   * verbatim (see ../pty/src/tags.ts:matchesAllTags). The TOML
   * inline-table quoted-key form (`"st.network" = "..."`) parses
   * as a literal `st.network` string key in smol-toml — verified
   * live before landing.
   *
   * Key spelled `st.network` exactly (pty-claude's choice — visible,
   * non-reserved). The dot is inside a quoted key so it's not
   * interpreted as a TOML dotted-key nested-table.
   */
  network: string;
}): string {
  // pty runs `command` via `sh -c`, so produce a shell command line by
  // space-joining shell-quoted argv elements. Then TOML-quote the
  // whole thing for the `command = "..."` value.
  const rawShellLine = opts.invocation.map(shellQuote).join(' ');
  // Only claude has the first-launch TUI gates; codex doesn't. The
  // poker also only makes sense under pty (nothing to `pty send` to
  // otherwise). buildPtyToml is only called on the pty path, so the
  // second condition is trivially met here.
  const shellLine =
    opts.unattended && opts.harness === 'claude'
      ? `${pokerPrefix(`${opts.identity}-${opts.sessionName}`)} exec ${rawShellLine}`
      : rawShellLine;
  // Prefix is the identity — pty namespace is global and repo
  // basenames can collide (`taskflow` in two different clones both
  // trying to be `taskflow-claude`). Identity is unique per agent by
  // construction, and it means a generic shepherd poking `pty send
  // <identity>-<session>` always finds the right session (pty joins
  // prefix + sessionName with a dash — see ../pty/src/ptyfile.ts:58).
  // Historic behavior was repo-basename via a `resolveRepoPrefix(cwd)`
  // walk; that shape is preserved by `--session-name` when a user
  // really wants a different key inside the identity namespace.
  const prefix = opts.identity;
  const lines: string[] = [];
  lines.push(`prefix = "${tomlEscape(prefix)}"`);
  lines.push('');
  lines.push(`[sessions.${opts.sessionName}]`);
  lines.push(`command = "${tomlEscape(shellLine)}"`);
  // Agent tags. The optional `strategy` value gets mirrored to the
  // ding sidecar below, so the launch always emits a matched pair
  // — no mixed-strategy pty.toml. `st.network` is always emitted
  // (see `opts.network` docstring): uniform inspection signal +
  // pty's `--filter-tag st.network=<v>` primitive for network-scoped
  // TUI/list operations.
  const networkTag = `"st.network" = "${tomlEscape(opts.network)}"`;
  if (opts.agentStrategy !== undefined) {
    lines.push(
      `tags = { role = "agent", strategy = "${tomlEscape(opts.agentStrategy)}", ${networkTag} }`
    );
  } else {
    lines.push(`tags = { role = "agent", ${networkTag} }`);
  }
  lines.push('');
  lines.push(`[sessions.${opts.sessionName}.env]`);
  lines.push(`ST_AGENT = "${tomlEscape(opts.identity)}"`);
  // Isolation robustness: pin the state root a `pty up`/`pty restart`
  // invocation would use, so a session launched under an explicit
  // `ST_ROOT` (isolation, eval harness, per-project tree, etc.) stays
  // on that root regardless of who later resurrects the pty session.
  // Only baked when the invoker explicitly set ST_ROOT/COORD_ROOT —
  // default-path launches leave the env unset so pty.toml doesn't
  // freeze today's default into future restarts. See the enclosing
  // opts.stRoot docstring for the resolution rule.
  if (opts.stRoot !== undefined) {
    lines.push(`ST_ROOT = "${tomlEscape(opts.stRoot)}"`);
  }
  if (opts.addDingSidecar) {
    // Emit `st ding` (post-cutover canonical). The `coord ding` form
    // still works because `coord` is a dual alias, but hardcoding
    // the legacy name into every fresh pty.toml perpetuates the
    // drift — future removal of the coord alias would silently
    // break every `st launch codex` config generated pre-fix. Users
    // regenerate their pty.toml (rm pty.toml && st launch codex …)
    // to pick up the new form.
    //
    // The target passed to `st ding` MUST be the fully-qualified
    // pty session name (`${prefix}-${sessionName}` = `${identity}-
    // ${sessionName}` per F3) — same reason the F1 auto-poker at
    // ~line 615 uses that form. pty joins prefix + sessionName with
    // a dash (see ../pty/src/ptyfile.ts:58); addressing the bare
    // sessionName from a `pty send` context returns
    // `Session "<x>" not found` and every poke silently fails.
    // Historic bug: this line used bare `opts.sessionName`, which
    // (a) silently mis-addressed every ding poke, and (b) was
    // masked by #62's startup-grace — the ding kept waiting for
    // the wrong name forever, looking healthy while delivering
    // nothing.
    const dingTarget = `${opts.identity}-${opts.sessionName}`;
    const dingLine = `st ding ${shellQuote(dingTarget)} --identity ${shellQuote(opts.identity)}`;
    lines.push('');
    lines.push(`[sessions.ding]`);
    lines.push(`command = "${tomlEscape(dingLine)}"`);
    // Ding tags — mirror the agent's strategy so the launch is
    // internally consistent. Historically the ding was hardcoded
    // to `strategy = "permanent"` while the agent had no strategy
    // tag — a mismatch that zombied the ding on `pty gc` after
    // the codex session died. Now: whatever the agent got, the
    // ding gets. `st.network` mirrors too — the whole launch is
    // one network, so the sidecar carries the same tag as the
    // main session.
    if (opts.agentStrategy !== undefined) {
      lines.push(
        `tags = { role = "ding", strategy = "${tomlEscape(opts.agentStrategy)}", ${networkTag} }`
      );
    } else {
      lines.push(`tags = { role = "ding", ${networkTag} }`);
    }
    lines.push('');
    lines.push(`[sessions.ding.env]`);
    lines.push(`ST_AGENT = "${tomlEscape(opts.identity)}"`);
    // Same rationale as the main session's env: pin the isolation
    // root so `pty restart <identity>-ding` doesn't drift back to
    // the live default state root.
    if (opts.stRoot !== undefined) {
      lines.push(`ST_ROOT = "${tomlEscape(opts.stRoot)}"`);
    }
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
  return resolveClaudeHooksDirWithHint().path;
}

/**
 * Discriminated variant of {@link resolveClaudeHooksDir} that names
 * the specific failure mode so the caller can emit a LOUD, actionable
 * stderr error instead of the historic silent soft-skip.
 *
 * Returns `{ path: <abs>, hint: null }` on success. On failure returns
 * `{ path: null, hint: "<what failed> ; <how to fix>" }`. The hint
 * quotes the paths inspected so an operator can grep their install
 * for the missing pieces.
 *
 * Historic behavior: `resolveClaudeHooksDir()` returned null in TWO
 * distinct failure modes (bin/shim not resolvable OR examples/ dir
 * missing) and the call site logged a single generic "hooks not found
 * on disk" message. Silent-ish → an operator misses it → launches
 * come up hookless (no boot ritual, no PreCompact, no StopFailure).
 * Johannes hit this. This function surfaces WHICH mode failed so
 * the caller's message can name it.
 */
export function resolveClaudeHooksDirWithHint(): {
  path: string | null;
  hint: string | null;
} {
  let coordBin: string;
  try {
    coordBin = resolveCoordBinPath();
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    return {
      path: null,
      hint:
        `could not resolve the smalltalk shim (bin/st) via package.json ` +
        `walk from this module + \`which st\` / \`which coord\` PATH ` +
        `lookup: ${msg}. Confirm you're launching from within a ` +
        `checkout that has @myobie/coord in its package.json chain, ` +
        `or that \`st\` is on your $PATH.`,
    };
  }
  const smalltalkRoot = dirname(dirname(coordBin));
  const hooksDir = join(smalltalkRoot, 'examples', 'claude-code', 'hooks');
  let stat: ReturnType<typeof statSync> | null = null;
  try {
    stat = statSync(hooksDir);
  } catch {
    // fall through to failure branch below
  }
  if (stat !== null && stat.isDirectory()) {
    return { path: hooksDir, hint: null };
  }
  return {
    path: null,
    hint:
      `resolved the smalltalk shim to ${coordBin} (root: ${smalltalkRoot}) ` +
      `but ${hooksDir} does not exist on disk. Your install may have ` +
      `skipped \`examples/\` (some npm install flavors do). Fix: from ` +
      `the smalltalk repo checkout, run \`npm install && npm link\` so ` +
      `the shipped hooks are present on the resolved path. Or pass ` +
      `\`--no-hooks\` to acknowledge and launch hookless intentionally.`,
  };
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
    // F2: opt every fresh Claude Code session into the project MCP
    // server this launch just wired up in `.mcp.json`, without the
    // "Enable the `st` MCP server?" gate blocking hands-off
    // standup. `enableAllProjectMcpServers` blanket-approves any
    // server in the project `.mcp.json`; `enabledMcpjsonServers`
    // pins the specific entry — `st` matches the post-cutover
    // mcpServers key that `st init` writes — so a future .mcp.json
    // edit that adds an unrelated server still requires an explicit
    // approval. Verified in the Claude Code settings docs
    // (code.claude.com/docs/en/settings) — both fields are
    // load-bearing.
    enableAllProjectMcpServers: true,
    enabledMcpjsonServers: ['st'],
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

// ─── ding-mode bus instructions install ────────────────────────────────

/**
 * The bus-mechanics contract delivered to a ding-mode agent — the
 * ding-mode analog of `src/mcp/capabilities.ts:CHANNEL_INSTRUCTIONS`
 * (which the MCP server sends as `instructions:` on the MCP
 * capabilities). Both blurbs are two versions of ONE bus contract:
 * MCP agents get one via the transport's `instructions:` field;
 * ding-mode agents get the other via an `@DING-BUS.md` import in
 * their entry file (CLAUDE.md).
 *
 * **Keep in sync**: when the *shared* bus contract changes (new
 * tools, new conventions, boot-ritual updates), update BOTH this
 * constant AND `CHANNEL_INSTRUCTIONS` in
 * `src/mcp/capabilities.ts`. A drift between the two means MCP
 * agents and ding-mode agents will behave differently for the
 * same protocol event.
 *
 * **Ding-only sections don't mirror to CHANNEL_INSTRUCTIONS.** The
 * "Propagate ding-mode through every spawn" section below is
 * transport-specific truth — it exists because ding-mode is a
 * *machine* property (MCP-hostile envs are hostile to every
 * process, not just the boot agent), so a child spawn must also
 * be ding-mode. MCP-mode has no equivalent constraint (an MCP
 * child on an MCP-capable machine is fine), so
 * CHANNEL_INSTRUCTIONS deliberately omits this section. Don't
 * add a matching-but-nonsensical section there.
 *
 * Content shape follows the same "boot ritual → arrivals-handling →
 * threads-stay-on-bus → tools inventory" structure as
 * CHANNEL_INSTRUCTIONS, adapted for the CLI-based delivery path,
 * plus the ding-only "propagate-through-spawns" section.
 */
export const DING_BUS_INSTRUCTIONS = `# Ding-mode bus instructions

You are connected to smalltalk via ding-mode (no MCP). Bus ops go
through the \`st\` CLI. **You will NOT receive \`<channel>\` blocks
— those are MCP-only.** Inbound messages arrive as \`[DING]\`
pokes in your terminal; always confirm the actual message via
\`st message ls\` + \`st message read\` before acting.

## Boot ritual (on cold start or /clear)

1. \`st status $ST_AGENT --set available\` — set your status file
   so peers see you as active.
2. Drain your inbox backlog: \`st message ls\` to enumerate
   filenames, then for each filename \`st message read <filename>\`,
   \`st message reply <filename> -m "<your reply>"\` if a response
   is warranted, and \`st message archive <filename>\` to clear.
   Don't leave inbox items unaddressed.
3. \`st agents --json --enrich\` to see who's around and whether
   any peers are waiting on you.

## Inbound message handling ([DING] pokes)

New peer messages surface in your terminal as \`[DING] new
smalltalk message: <subject> (from <sender>); check your inbox\`
lines. For each poke:

1. \`st message ls\` to find the new filename (or read it out of
   the \`[DING]\` line's subject if unambiguous).
2. \`st message read <filename>\` to fetch the body + frontmatter.
3. \`st message reply <filename> -m "<your reply>"\` if a response
   is warranted (the CLI derives the recipient from the message's
   \`from:\` frontmatter and threads the reply).
4. \`st message archive <filename>\` to clear the inbox.

The tidy-check tick may also poke you with \`[DING] tidy-check:
inbox=<n> (oldest <age>).\` — that means your inbox has drifted
past the staleness threshold. Drain the backlog above.

## Threads stay on the bus

A thread that originated from a \`[DING]\` poke or an inbox
message is conversed *only* via \`st message send\` /
\`st message reply\` — questions, clarifications, blockers, "I
think I'm done" signals, follow-up thoughts, all of it. By
default, your pty REPL is unattended — no human is watching what
you print to your own screen. Your correspondent is your
interlocutor for the thread; they will relay anything that
matters to the user. If you would otherwise pause to ask "should
I do X?" at your REPL, send it via \`st message reply\` instead.
The only time it's right to address the REPL is when a human
directly typed there.

## Propagate ding-mode through every spawn

The presence of THIS file (DING-BUS.md) tells you that you are
in ding-mode: this machine is MCP-hostile (some sandboxed
runners, some corporate-managed setups, some Claude Code
distributions where MCP is disabled). ding-mode is not a
per-agent choice — it's a property of the whole machine, so
**every agent you spawn on this machine MUST also be in
ding-mode.**

When you \`st launch\` a specialist / worker / supervisor / any
child agent, you MUST pass \`--ding\`:

\`\`\`sh
st launch <harness> --identity <child-id> --ding \\
  [--persona <path>] [--permanent] [other flags…]
\`\`\`

The child gets its own DING-BUS.md and CLAUDE.md wiring
automatically (\`st launch --ding\` installs the same contract in
the child's cwd), so the "propagate ding through every spawn"
rule holds at every level of a cos → supervisor → worker tree.

Do NOT run plain \`st launch <harness> …\` (no \`--ding\`) — that
spawns an MCP-mode child, which will fail to start (or worse:
appear to start and then never deliver on the MCP transport this
machine can't support). A mixed-mode tree — you in ding-mode,
child in MCP-mode — is always a bug on this machine.

Rule of thumb: **you can copy your own launch pattern from the
persona / onboarding docs, but you must add \`--ding\` to any
copied \`st launch …\` command that doesn't already have it.**

## CLI inventory

Bus ops:
- \`st message send <to> [-m <body> | --message <body>] [--subject S] [--in-reply-to F] [--tags T,T] [--priority P]\`
- \`st message reply <filename> -m <body> [--subject S]\`
- \`st message ls [<identity>] [--archive] [--count | --json] [--since UNIX_MS] [--from ID] [--orphans]\`
- \`st message read [<identity>] <filename> [--raw | --json] [--archive]\`
- \`st message archive [<identity>] <filename> [--with-attachments]\`
- \`st message thread [<identity>] <filename> [--tree]\`

Peer discovery + state:
- \`st agents [--status STATE] [--json [--enrich]]\`
- \`st status [<identity>] [--set <state>]\`

Working state (lossless-restart):
- \`st context read [<identity>] [--decisions | --full]\`
- \`st context write [<identity>]\` (reads new content from stdin)
- \`st context append [<identity>] --decision "<text>" --why "<text>"\`

Spawning children (MUST include \`--ding\` on this machine — see
"Propagate ding-mode through every spawn" above):
- \`st launch <harness> --identity <id> --ding [--persona <path>] [--permanent] [--permission-mode <mode>] [--agent <bin>] [--session-name <name>]\`

Every command supports \`--help\` for the full flag surface.
`;

/**
 * Do (or plan) the DING-BUS.md install. Same mechanics as
 * {@link installPersona}: copy content to \`<cwd>/DING-BUS.md\`,
 * surgically append \`@DING-BUS.md\` to CLAUDE.md if not already
 * present, add DING-BUS.md to \`.git/info/exclude\`.
 *
 * Only runs when \`--ding\` was passed on a claude launch — ding-mode
 * is claude-only (codex already has its own instructions path).
 */
function installDingBusInstructions(opts: {
  cwd: string;
  dryRun: boolean;
}): DingBusInstallResult {
  const { cwd, dryRun } = opts;
  const entryFile = 'CLAUDE.md' as const;
  const entryFilePath = join(cwd, entryFile);
  const dingBusMdPath = join(cwd, 'DING-BUS.md');

  // Plan the entry-file edit. Match `@DING-BUS.md` on its own line
  // (leniently — trailing space or trailing comment counts as
  // already-present).
  let existingText = '';
  let fileExisted = false;
  try {
    existingText = readFileSync(entryFilePath, 'utf8');
    fileExisted = true;
  } catch {
    // missing → we'll create it
  }
  const importPattern = /^@DING-BUS\.md\b/m;
  const importAlreadyPresent = importPattern.test(existingText);
  const importLineToAppend = !importAlreadyPresent;
  const entryFileCreated = !fileExisted && importLineToAppend;

  // Git-exclude the files we own. DING-BUS.md is always excluded.
  // The entryFile is only excluded when WE created it — a
  // pre-existing repo CLAUDE.md is left in the repo's tracking.
  const wantedExcludes: string[] = ['DING-BUS.md'];
  if (entryFileCreated) wantedExcludes.push(entryFile);

  const excludeInfo = readGitExclude(cwd);
  const gitRepoAbsent = excludeInfo === null;
  const gitExcludeEntriesAdded = excludeInfo
    ? missingExcludeEntries(excludeInfo.text, wantedExcludes)
    : [];

  if (!dryRun) {
    // Write DING-BUS.md — always overwrite because we own the
    // content (it's a versioned constant, not user data).
    writeFileSync(dingBusMdPath, DING_BUS_INSTRUCTIONS);

    // Entry-file edit (mirrors installPersona logic).
    if (importLineToAppend) {
      if (!fileExisted) {
        writeFileSync(entryFilePath, '@DING-BUS.md\n');
      } else {
        const sep =
          existingText.length === 0 || existingText.endsWith('\n')
            ? ''
            : '\n';
        writeFileSync(
          entryFilePath,
          existingText + sep + '@DING-BUS.md\n'
        );
      }
    }

    // Git-exclude append.
    if (excludeInfo && gitExcludeEntriesAdded.length > 0) {
      appendGitExclude(
        excludeInfo.path,
        excludeInfo.text,
        gitExcludeEntriesAdded
      );
    }
  }

  return {
    dingBusMdPath,
    entryFile,
    entryFilePath,
    entryFileCreated,
    importLineAppended: importLineToAppend,
    gitExcludeEntriesAdded,
    gitRepoAbsent,
  };
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

/** Read the canonical `info/exclude` for `cwd`'s git repo (or empty
 *  string if the file doesn't exist yet). Returns null when `cwd`
 *  isn't a git-tracked directory.
 *
 *  Uses `git rev-parse --git-path info/exclude` rather than the naive
 *  `join(cwd, '.git', 'info', 'exclude')`. The naive form breaks in
 *  worktrees, where `<worktree>/.git` is a text file pointing at the
 *  real git dir (`.git/worktrees/<name>`) — statting it as a dir
 *  returns false and we historically bailed with `gitRepoAbsent:
 *  true`, silently skipping the git-exclude append for anyone working
 *  in a worktree. `git rev-parse --git-path` normalizes across:
 *  - Regular repo → `<cwd>/.git/info/exclude`.
 *  - Worktree → the SHARED main-repo `<main>/.git/info/exclude`
 *    (info/exclude is shared across worktrees per git's design).
 *  - Bare repo → the repo's `info/exclude`.
 *  - Non-git dir → nonzero exit → we return null. */
function readGitExclude(cwd: string): { path: string; text: string } | null {
  const r = spawnSync(
    'git',
    ['-C', cwd, 'rev-parse', '--git-path', 'info/exclude'],
    { encoding: 'utf8' }
  );
  if (r.status !== 0 || typeof r.stdout !== 'string') return null;
  const raw = r.stdout.trim();
  if (raw.length === 0) return null;
  // git returns a path relative to cwd for a regular repo, absolute
  // for a worktree/GIT_DIR override. Resolve to absolute for the
  // downstream write path.
  const excludePath = isAbsolute(raw) ? raw : join(cwd, raw);
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
  // Ding-mode: skip MCP wiring entirely. The claude agent joins the
  // network via `st ding` sidecar + `st` CLI, same as codex. Load-
  // bearing for environments where MCP servers can't run at all
  // (Johannes's setup, some sandboxes).
  const ding = input.ding === true;
  // Channel resolution: forced off in ding-mode (there's no MCP to
  // push through). Otherwise unchanged — claude defaults on, codex
  // off, `--no-channel` forces off.
  const channel = ding
    ? false
    : input.noChannel === true
      ? false
      : harness === 'claude';
  const mcpJsonPath = join(cwd, '.mcp.json');
  if (!input.dryRun && !ding) {
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
  // Warn if ding-mode was requested but a stale .mcp.json exists in
  // the cwd — an operator switching from MCP to ding-mode almost
  // certainly wants the old file gone. Advisory, not a hard error.
  if (ding && existsSync(mcpJsonPath)) {
    ctx.stderr(
      `[smalltalk launch] --ding: existing .mcp.json in ${cwd} left as-is. ` +
        `Delete it (rm .mcp.json) if this was a switch from MCP-mode to ` +
        `ding-mode; a stale .mcp.json will still be read by Claude Code ` +
        `and may try to spin up an MCP server ding-mode assumed you didn't ` +
        `want.\n`
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

  // ─── DING-BUS.md install (ding-mode claude only) ────────────────────
  // The ding-mode analog of the MCP CHANNEL_INSTRUCTIONS blurb. Auto-
  // installed with `--ding` because ding-mode agents have no MCP
  // transport to receive `instructions:` through — the persona file
  // provides role, this file provides bus mechanics. Composes with
  // `@PERSONA.md` via the same entry-file `@`-import mechanism.
  let dingBusResult: DingBusInstallResult | null = null;
  if (ding && harness === 'claude') {
    dingBusResult = installDingBusInstructions({
      cwd,
      dryRun: input.dryRun === true,
    });
    if (dingBusResult.gitRepoAbsent) {
      ctx.stderr(
        `[smalltalk launch] --ding: ${cwd} is not a git repo; ` +
          `skipping .git/info/exclude update. DING-BUS.md is still ` +
          `installed.\n`
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
  // env (parity with pty-claude-launcher.sh's fallback) > shape-aware
  // default. Value is passed through to claude verbatim — we don't
  // validate here (claude rejects unknown modes loudly, and
  // duplicating the enum would just rot).
  //
  // Shape-aware default (Nathan's 3-tier hierarchy):
  //   - spawner-shaped launch (cos, supervisor): default
  //     `bypassPermissions`, because claude's `auto` mode classifier
  //     hard-blocks a spawner from creating autonomous agents — the
  //     regression Johannes's pty.toml surfaced.
  //   - anything else (workers, plain agents): default `auto`. Auto
  //     is correct + safe for a leaf agent that does work but
  //     doesn't spawn.
  //
  // Existing callers with explicit flags or `$CLAUDE_PERMISSION_MODE`
  // set are byte-identical either way — the shape-aware branch only
  // fires when neither is set.
  const spawnerShaped = isSpawnerShaped(identity, input.persona);
  const permissionMode: string =
    input.permissionMode !== undefined && input.permissionMode.length > 0
      ? input.permissionMode
      : input.env.CLAUDE_PERMISSION_MODE !== undefined &&
          input.env.CLAUDE_PERMISSION_MODE.length > 0
        ? input.env.CLAUDE_PERMISSION_MODE
        : spawnerShaped
          ? 'bypassPermissions'
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
  // F1: resolve unattended mode. Explicit `--unattended` (input.
  // unattended === true) wins. Otherwise auto-on when the caller's
  // stdin is definitively not a TTY — a CoS shelling out via
  // spawn() reports stdinIsTty()=false and gets hands-off standup
  // for free. When stdinIsTty is absent (older CliContext callers,
  // early tests) we can't tell → default to attended.
  const stdinNotTty =
    ctx.stdinIsTty !== undefined && ctx.stdinIsTty() === false;
  const unattended =
    input.unattended === true || (input.unattended === undefined && stdinNotTty);
  // Isolation-robust `ST_ROOT` propagation: when the invoker had
  // ST_ROOT (or legacy COORD_ROOT) explicitly set, bake the resolved
  // absolute path into the generated pty.toml's `[sessions.*.env]`.
  // `input.coordRoot` is already the resolved value (via
  // `coordRootFrom(ctx.env)` at the CLI layer), so this pins the
  // exact tree the launch is scoping to — a later `pty up` /
  // `pty restart` / `pty gc` resurrection from any shell will keep
  // the isolation instead of falling back to the live default root.
  // Default-path launches leave the env unset so pty.toml doesn't
  // freeze today's default into future restarts.
  const stRootForSession: string | undefined =
    input.env.ST_ROOT !== undefined || input.env.COORD_ROOT !== undefined
      ? input.coordRoot
      : undefined;
  // --permanent: plumbs through the buildPtyToml `agentStrategy?`
  // future-proof hook that landed with the ding fix. When set,
  // BOTH the agent session AND the ding sidecar get `strategy =
  // "permanent"` — a launch is either ephemeral or it isn't; no
  // mixed-strategy pty.toml.
  const permanent = input.permanent === true;
  const agentStrategy: string | undefined = permanent
    ? 'permanent'
    : undefined;
  // Footgun-guard: a silently-reap-able CoS is a nasty non-obvious
  // failure for a newcomer following the onboarding docs. When a
  // CoS-shaped launch (identity 'cos' OR a persona whose basename
  // is chief-of-staff.md) omits --permanent, warn to stderr.
  // Detection stays narrow so we don't spam the eval-cell launches
  // that legitimately want ephemeral CoS-lookalikes.
  // Footgun-guard: warn when a spawner-shaped launch (cos or
  // supervisor) omits --permanent — a silently-reap-able spawner
  // under pty gc is a nasty non-obvious failure for a newcomer
  // following the onboarding docs. Ephemeral eval spawners
  // intentionally decline permanent; the warning is opt-in
  // acknowledgment, not a hard block. Reuses the same
  // spawner-shape detection as the permission-mode default above.
  if (!permanent && spawnerShaped) {
    ctx.stderr(
      `[smalltalk launch] launching a spawner (cos/supervisor) without ` +
        `--permanent; pty gc may reap it under idle-cleanup. If this is ` +
        `a production spawner (not an eval/test spin-up), pass ` +
        `--permanent so the launch bakes strategy = "permanent" into ` +
        `pty.toml.\n`
    );
  }
  const ptyTomlPath = join(cwd, 'pty.toml');
  let ptyTomlPreview: string | null = null;
  if (usedPty) {
    const preview = buildPtyToml({
      harness,
      sessionName,
      identity,
      invocation: argv,
      // Ding sidecar: always for codex (its default), always for
      // claude in ding-mode. `st ding <sess> --identity <id>` watches
      // the identity's inbox and pty-sends notifications into the
      // agent's terminal.
      addDingSidecar: harness === 'codex' || ding,
      unattended,
      stRoot: stRootForSession,
      agentStrategy,
      // Phase-1 pty isolation label. Always emitted (unlike
      // stRoot, which conditionally emits the ST_ROOT env line);
      // input.coordRoot is resolved at the CLI layer and always
      // a valid path.
      network: input.coordRoot,
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
      // Ding sidecar: always for codex (its default), always for
      // claude in ding-mode. `st ding <sess> --identity <id>` watches
      // the identity's inbox and pty-sends notifications into the
      // agent's terminal.
      addDingSidecar: harness === 'codex' || ding,
      unattended,
      stRoot: stRootForSession,
      agentStrategy,
      // Phase-1 pty isolation label. Always emitted (unlike
      // stRoot, which conditionally emits the ST_ROOT env line);
      // input.coordRoot is resolved at the CLI layer and always
      // a valid path.
      network: input.coordRoot,
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
    let hooksHint: string | null = null;
    if (input.hooksDir !== undefined) {
      // Explicit override — verify the path still exists.
      if (existsSync(input.hooksDir)) {
        hooksDir = input.hooksDir;
      } else {
        hooksDir = null;
        hooksHint =
          `explicit \`--hooksDir\`/input.hooksDir override ${input.hooksDir} ` +
          `does not exist on disk. Pass a valid path, remove the override to ` +
          `use auto-resolution, or pass \`--no-hooks\` to launch hookless ` +
          `intentionally.`;
      }
    } else {
      const resolved = resolveClaudeHooksDirWithHint();
      hooksDir = resolved.path;
      hooksHint = resolved.hint;
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
      // LOUD failure: the historic silent soft-skip meant Johannes's
      // Claude launched hookless (no boot ritual, no PreCompact, no
      // StopFailure) without an obvious signal. Now we emit a full
      // multi-line error naming the specific failure mode + how to
      // fix it. Launch still proceeds — hookless is degraded but
      // not fatal — but the operator gets a chance to notice.
      const hintForOperator =
        hooksHint ?? 'unknown resolution failure (please file a bug).';
      ctx.stderr(
        `\n` +
          `[smalltalk launch] ────────────────────────────────────────\n` +
          `[smalltalk launch] Claude Code hooks NOT installed. The\n` +
          `[smalltalk launch] boot ritual, PreCompact flush, and\n` +
          `[smalltalk launch] StopFailure ding hooks will NOT fire\n` +
          `[smalltalk launch] for this session.\n` +
          `[smalltalk launch]\n` +
          `[smalltalk launch] Why: ${hintForOperator}\n` +
          `[smalltalk launch]\n` +
          `[smalltalk launch] The launch will continue, but the agent\n` +
          `[smalltalk launch] will come up hookless (no session-start\n` +
          `[smalltalk launch] rehydrate, no post-compaction stub, no\n` +
          `[smalltalk launch] API-error visibility). Fix and re-run,\n` +
          `[smalltalk launch] or pass --no-hooks to acknowledge and\n` +
          `[smalltalk launch] silence this warning.\n` +
          `[smalltalk launch] ────────────────────────────────────────\n\n`
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
      unattended,
      permanent,
      ding,
      claudeSettingsPath,
      claudeSettingsPreview,
      persona: personaResult,
      dingBus: dingBusResult,
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
    // pty picks the sessions up from pty.toml.
    //
    // Attended launches inherit stdio + wait for exit so the operator
    // gets pty up's interactive UI (attach, "ctrl+b to run in
    // background", etc). Unattended launches — CoS spawning a
    // specialist under `spawn()` — background pty up instead:
    // detached, stdio ignored, `unref`'d so the parent st launch
    // process returns immediately. Without this, `pty up` blocks
    // ~47s+ waiting on its interactive attach, and a CoS caller
    // reads that as a hang.
    //
    // F4: auto-detect unattended via `stdinIsTty()` false (mirrors
    // F1's --unattended resolution). Callers passing --attended or
    // running with a real TTY keep the current inherit-and-wait
    // behavior.
    const spawnUnattended =
      ctx.stdinIsTty !== undefined && ctx.stdinIsTty() === false;
    if (spawnUnattended) {
      const child = spawn(ptyPath, ['up'], {
        detached: true,
        stdio: 'ignore',
        env: spawnEnv,
        cwd,
      });
      child.unref();
    } else {
      const child = spawn(ptyPath, ['up'], {
        stdio: 'inherit',
        env: spawnEnv,
        cwd,
      });
      await new Promise<void>((resolvePromise) => {
        child.on('exit', () => resolvePromise());
      });
    }
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
    unattended,
    permanent,
    ding,
    claudeSettingsPath,
    claudeSettingsPreview,
    persona: personaResult,
    dingBus: dingBusResult,
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
  '  --unattended            Bake a startup auto-poker into the pty session\n' +
  '                          command so Claude Code\'s first-launch TUI\n' +
  '                          gates (workspace trust, --dangerously-load-\n' +
  '                          development-channels warning, resume-mode\n' +
  '                          dialog) each receive an Enter without a human\n' +
  '                          at the REPL. Auto-on when stdin is not a TTY\n' +
  '                          (a CoS spawning a specialist via `spawn` gets\n' +
  '                          it for free). Only affects the claude harness\n' +
  '                          + pty path; codex and --no-pty are unaffected.\n' +
  '  --attended              Force attended even when stdin is not a TTY —\n' +
  '                          escape hatch for headless debug runs that want\n' +
  '                          the human-driven dialog experience.\n' +
  '  --permanent             Bake `strategy = "permanent"` into the generated\n' +
  '                          pty.toml for BOTH the agent session AND (codex)\n' +
  '                          the ding sidecar, so pty resurrects them if\n' +
  '                          their daemons die and `pty gc` doesn\'t reap\n' +
  '                          them under idle-cleanup. Required for a\n' +
  '                          production CoS or any always-on agent. Omitted\n' +
  '                          → both sessions are ephemeral (pty\'s default).\n' +
  '                          Warns if a CoS-shaped launch (--identity cos\n' +
  '                          or chief-of-staff persona) is missing.\n' +
  '  --ding                  Launch claude codex-style: no MCP wiring\n' +
  '                          (skip .mcp.json entirely, no --channel flag),\n' +
  '                          add an `st ding` sidecar for inbox delivery,\n' +
  '                          agent uses the `st` CLI for all bus ops.\n' +
  '                          Load-bearing for environments where MCP\n' +
  '                          servers can\'t run at all. Hooks\n' +
  '                          (.claude/settings.local.json) are still\n' +
  '                          generated — the boot ritual + PreCompact\n' +
  '                          flush + StopFailure ding are Claude Code\n' +
  '                          hooks, MCP-independent. No-op for codex\n' +
  '                          (already ding-mode by default).\n' +
  '  --dry-run               Print what would happen; touch nothing.\n' +
  '  --print                 Alias for --dry-run.\n\n' +
  '  Examples:\n' +
  '    st launch claude                              # anon identity, channel mode + boot hooks\n' +
  '    st launch claude --identity alice             # persistent identity\n' +
  '    st launch claude --identity cos --permanent \\\n' +
  '        --persona ~/src/github.com/myobie/personas/chief-of-staff.md\n' +
  '                                                  # production CoS (MCP mode)\n' +
  '    st launch claude --identity cos --permanent --ding \\\n' +
  '        --persona ~/src/github.com/myobie/personas/chief-of-staff.md\n' +
  '                                                  # production CoS (ding-mode; no MCP)\n' +
  '    st launch claude --no-hooks                   # skip .claude/settings.local.json\n' +
  '    st launch codex                               # + st ding sidecar\n' +
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
  let unattendedFlag: boolean | undefined;
  let permanent = false;
  let ding = false;
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
      case '--unattended':
        unattendedFlag = true;
        break;
      case '--attended':
        // Escape hatch: force attended even when stdin isn't a TTY
        // (e.g. a headless CI test run that still wants the human-
        // driven dialog experience for debugging).
        unattendedFlag = false;
        break;
      case '--permanent':
        permanent = true;
        break;
      case '--ding':
        ding = true;
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
      ...(unattendedFlag !== undefined && { unattended: unattendedFlag }),
      permanent,
      ding,
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
    ctx.stdout(`unattended:     ${r.unattended ? 'yes' : 'no'}\n`);
    ctx.stdout(`permanent:      ${r.permanent ? 'yes' : 'no'}\n`);
    ctx.stdout(`ding mode:      ${r.ding ? 'yes' : 'no'}\n`);
    if (r.ding) {
      ctx.stdout(`mcp.json:       (skipped — ding mode)\n`);
    } else {
      ctx.stdout(`mcp.json:       ${r.mcpJsonPath}\n`);
    }
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
    if (r.dingBus !== null) {
      // DING-BUS.md summary — same shape as the persona block, so an
      // operator debugging a `--ding` launch sees exactly what the
      // install landed alongside the persona.
      ctx.stdout(`\nding-bus:\n`);
      ctx.stdout(`  write:        ${r.dingBus.dingBusMdPath}\n`);
      const action = r.dingBus.entryFileCreated
        ? 'create'
        : r.dingBus.importLineAppended
          ? 'append @DING-BUS.md'
          : 'no change (import already present)';
      ctx.stdout(`  entry file:   ${r.dingBus.entryFilePath} (${action})\n`);
      if (r.dingBus.gitRepoAbsent) {
        ctx.stdout(
          `  git-exclude:  skipped (${cwdForGitExcludeNote(r)} is not a git repo)\n`
        );
      } else if (r.dingBus.gitExcludeEntriesAdded.length === 0) {
        ctx.stdout(`  git-exclude:  no new entries (all already present)\n`);
      } else {
        ctx.stdout(
          `  git-exclude:  ${r.dingBus.gitExcludeEntriesAdded.join(', ')}\n`
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
