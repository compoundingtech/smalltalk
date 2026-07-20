// commands/hooks.ts — `st hooks path`: the read-only interface to st's
// agent-integration hooks (Claude Code, Codex, pi).
//
// Why this exists: the hooks are st's — co-versioned with the st CLI they call
// (they invoke st via $ST_BIN). Consumers (e.g. `convoy doctor`) used to reach
// into `examples/<family>/…` by ABSOLUTE PATH to find + verify them, which is
// fragile (false "hooks not present" when st isn't on PATH even though the
// hooks are installed + working via ST_BIN). This command is the stable
// interface: ask st where its hooks live and how to wire them.
//
// HARD CONSTRAINT (Nathan, 2026-07-20): it MODIFIES NOTHING. No auto-install,
// no writing settings, zero side effects. It only reads its own example dir and
// PRINTS: the hook-scripts location + the exact install config for the user (or
// a doctor) to apply themselves. Read-only, informational.

import { existsSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { invokedName, type CliContext } from '../cli-context.ts';

export type HookFamily = 'claude-code' | 'codex' | 'pi';
const FAMILIES: readonly HookFamily[] = ['claude-code', 'codex', 'pi'];
const DEFAULT_FAMILY: HookFamily = 'claude-code';

/**
 * Walk up from this module to the `@compoundingtech/smalltalk` package root and return it,
 * or null if not found. Mirrors {@link resolveStShimPath} in init.ts — robust
 * to symlinks + nesting depth, and never hardcodes a developer-machine path
 * (brief-026 boundary). Because `st` runs from its source checkout, the
 * returned root is where the example hook scripts actually live.
 */
function resolveRepoRoot(): string | null {
  let dir = dirname(fileURLToPath(import.meta.url));
  for (let i = 0; i < 16; i++) {
    const pkg = join(dir, 'package.json');
    if (existsSync(pkg)) {
      try {
        const parsed = JSON.parse(readFileSync(pkg, 'utf8')) as { name?: string };
        if (parsed.name === '@compoundingtech/smalltalk') return dir;
      } catch {
        // unreadable/invalid package.json — keep walking up
      }
    }
    const parent = dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  return null;
}

/** One hook script and whether it currently exists on disk. */
export interface HookScript {
  /** e.g. `session-start.sh` */
  name: string;
  /** absolute path where the script lives */
  path: string;
  /** what this hook does (human blurb) */
  role: string;
  /** false if the file is missing (surfaced, never auto-fixed) */
  present: boolean;
}

export interface HooksInfo {
  family: HookFamily;
  /** absolute directory holding the family's hook scripts */
  hooksDir: string;
  /**
   * The canonical st binary the hooks invoke via `$ST_BIN` (the `bin/st`
   * shim in this install), or null if not found. Consumers can report this
   * as the ST_BIN to bake into the hook environment.
   */
  stBin: string | null;
  /**
   * True iff every listed script exists on disk. NOTE: this means the hook
   * SCRIPTS are present in st's install — NOT that they are wired into a given
   * agent's settings. Whether an agent has them wired is the consumer's own
   * check against that agent's settings file, using {@link settings} / the
   * script paths here.
   */
  scriptsPresent: boolean;
  scripts: HookScript[];
  /** where the user wires the config (informational target) */
  settingsTarget: string;
  /**
   * claude-code: the ready-to-merge `hooks` block with absolute `command`
   * paths already filled in. Undefined for families whose config isn't a
   * single JSON block (codex = TOML, pi = extension auto-discovery).
   */
  settings?: unknown;
  /** codex/pi: the example config fragment to adapt (absolute path). */
  exampleConfig?: string;
  /** short human guidance for this family. */
  note: string;
}

interface FamilySpec {
  hooksSubdir: string;
  scripts: { name: string; role: string }[];
  settingsTarget: string;
  exampleConfigName?: string;
  buildSettings?: (hooksDir: string) => unknown;
  note: string;
}

const SPECS: Record<HookFamily, FamilySpec> = {
  'claude-code': {
    hooksSubdir: join('examples', 'claude-code', 'hooks'),
    scripts: [
      { name: 'session-start.sh', role: 'SessionStart — run the boot ritual on every session boundary' },
      { name: 'pre-compact.sh', role: 'PreCompact — flush working state before a compaction' },
      { name: 'pre-compact.impl.sh', role: 'support — the implementation pre-compact.sh execs (must be present too)' },
      { name: 'stop-failure.sh', role: 'StopFailure — surface an API-error wedge to the operator' },
    ],
    settingsTarget: '.claude/settings.local.json (gitignored; create if absent)',
    buildSettings: (hooksDir) => ({
      hooks: {
        SessionStart: [
          {
            hooks: [
              {
                type: 'command',
                async: true,
                asyncRewake: true,
                command: join(hooksDir, 'session-start.sh'),
              },
            ],
          },
        ],
        PreCompact: [
          { hooks: [{ type: 'command', command: join(hooksDir, 'pre-compact.sh') }] },
        ],
        StopFailure: [
          { hooks: [{ type: 'command', command: join(hooksDir, 'stop-failure.sh') }] },
        ],
      },
    }),
    note: 'Merge the "hooks" block below into .claude/settings.local.json. The scripts call st via $ST_BIN (an absolute path), so they work even when st is not on PATH.',
  },
  codex: {
    hooksSubdir: join('examples', 'codex'),
    scripts: [
      { name: 'session-start.sh', role: 'session-start — boot ritual' },
      { name: 'stop.sh', role: 'stop — end-of-turn handling' },
    ],
    settingsTarget: '~/.codex/config.toml',
    exampleConfigName: 'config.toml.example',
    note: 'Codex config is TOML, not a single JSON block. Adapt the fragment at exampleConfig into ~/.codex/config.toml, pointing the hook `command` paths at the scripts in hooksDir.',
  },
  pi: {
    hooksSubdir: join('examples', 'pi'),
    scripts: [{ name: 'smalltalk.ts', role: 'pi extension — registers the st verbs + push notifications' }],
    settingsTarget: '~/.pi/agent/extensions/smalltalk.ts (auto-discovered) or a settings.json "extensions" entry',
    exampleConfigName: 'settings.example.json',
    note: 'Pi auto-discovers ~/.pi/agent/extensions/*.ts. Symlink or copy smalltalk.ts (in hooksDir) there, or reference it via the settings fragment at exampleConfig.',
  },
};

/** Build the {@link HooksInfo} for a family, or null if the repo root (hence
 *  the example scripts) can't be located. Pure read: stats files, no writes.
 *  `rootArg` overrides the resolved package root (used by tests); omit it in
 *  normal use to auto-locate the @compoundingtech/smalltalk install. */
export function resolveHooksInfo(
  family: HookFamily,
  rootArg?: string | null
): HooksInfo | null {
  const root = rootArg !== undefined ? rootArg : resolveRepoRoot();
  if (root === null) return null;
  const spec = SPECS[family];
  const hooksDir = join(root, spec.hooksSubdir);
  const scripts: HookScript[] = spec.scripts.map((s) => {
    const path = join(hooksDir, s.name);
    return { name: s.name, path, role: s.role, present: existsSync(path) };
  });
  const stBinCandidate = join(root, 'bin', 'st');
  const info: HooksInfo = {
    family,
    hooksDir,
    stBin: existsSync(stBinCandidate) ? stBinCandidate : null,
    scriptsPresent: scripts.every((s) => s.present),
    scripts,
    settingsTarget: spec.settingsTarget,
    note: spec.note,
  };
  if (spec.buildSettings) info.settings = spec.buildSettings(hooksDir);
  if (spec.exampleConfigName) {
    info.exampleConfig = join(hooksDir, spec.exampleConfigName);
  }
  return info;
}

function renderHuman(info: HooksInfo, name: string): string {
  const lines: string[] = [];
  lines.push(`# st ${info.family} integration hooks`);
  lines.push('');
  lines.push(`Hook scripts: ${info.hooksDir}`);
  for (const s of info.scripts) {
    const mark = s.present ? '' : '   [MISSING]';
    lines.push(`  ${s.name}${mark}  — ${s.role}`);
  }
  lines.push(`st binary (ST_BIN): ${info.stBin ?? '(not found)'}`);
  lines.push(`scripts present: ${info.scriptsPresent ? 'yes' : 'NO — some scripts missing (see above)'}`);
  lines.push('');
  lines.push(`Install into: ${info.settingsTarget}`);
  lines.push(info.note);
  lines.push('');
  if (info.settings !== undefined) {
    lines.push('Config to merge:');
    lines.push(JSON.stringify(info.settings, null, 2));
  } else if (info.exampleConfig !== undefined) {
    lines.push(`Config fragment to adapt: ${info.exampleConfig}`);
  }
  lines.push('');
  lines.push(`(read-only: this printed the config; it changed nothing. Apply it yourself. \`${name} hooks path --json\` for a machine-readable form.)`);
  return lines.join('\n') + '\n';
}

export function cmdHooksCli(args: readonly string[], ctx: CliContext): number {
  const name = invokedName(ctx.env);
  const usage =
    `usage: ${name} hooks path [--for claude-code|codex|pi] [--json]\n\n` +
    "  Print where st's agent-integration hook scripts live and the exact\n" +
    '  config to install them. READ-ONLY: prints only, modifies nothing, never\n' +
    '  auto-installs. --for selects the runtime family (default claude-code).\n' +
    '  --json emits a machine-readable form (for tools like convoy doctor).\n';

  const sub = args[0];
  if (sub === undefined || sub === '-h' || sub === '--help') {
    ctx.stderr(usage);
    return sub === undefined ? 1 : 0;
  }
  if (sub !== 'path') {
    ctx.stderr(`unknown subcommand: ${sub}\n${usage}`);
    return 1;
  }

  let family: HookFamily = DEFAULT_FAMILY;
  let json = false;
  const rest = args.slice(1);
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i]!;
    switch (a) {
      case '--for': {
        const v = rest[++i];
        if (v === undefined) throw new Error('--for requires a value');
        if (!FAMILIES.includes(v as HookFamily)) {
          throw new Error(`--for must be one of ${FAMILIES.join('|')}, got: ${v}`);
        }
        family = v as HookFamily;
        break;
      }
      case '--json':
        json = true;
        break;
      case '-h':
      case '--help':
        ctx.stderr(usage);
        return 0;
      default:
        throw new Error(`unknown flag: ${a}`);
    }
  }

  const info = resolveHooksInfo(family);
  if (info === null) {
    ctx.stderr(
      `${name} hooks: could not locate the st (@compoundingtech/smalltalk) install root from ${import.meta.url}\n`
    );
    return 1;
  }

  if (json) {
    ctx.stdout(`${JSON.stringify(info, null, 2)}\n`);
  } else {
    ctx.stdout(renderHuman(info, name));
  }
  return 0;
}
