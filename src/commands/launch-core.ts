// commands/launch-core.ts — `st __launch-core`, the hidden
// JSON-in/JSON-out entrypoint convoy uses to reach the launch write
// logic without depending on the `st launch` user CLI surface.
//
// Contract (stable, additive-only):
// - STDIN: a JSON body of `LaunchInput` (minus `env` and `coordRoot`,
//   which come from the invoker's process env and the resolved
//   coordRoot chain — same resolution as any other `st` command).
//   Unknown fields are IGNORED (forward-compat when older
//   consumers send a subset).
// - STDOUT: a JSON body of `LaunchResult` (the same struct
//   `cmdLaunch` returns) on exit-0.
// - STDERR: error messages on non-zero exit.
// - EXIT CODE:
//     0 = success (LaunchResult on stdout)
//     1 = validation error (input JSON malformed, or harness is not
//         'claude' | 'codex', or a required field is missing)
//     2 = internal error (unexpected exception during launch)
//
// NOT LISTED in `st help` / `st --help` / completions. Only reachable
// by name. This is the convoy bridge, not a user-facing verb — the
// end state is convoy porting the write logic to Swift and
// retiring this entrypoint entirely (parity-tested against its
// output).

import type { CliContext } from '../cli-context.ts';

import { cmdLaunch, type Harness, type LaunchInput, type LaunchResult } from './launch.ts';

/**
 * Whitelist of harness values — the JSON parser is untrusted so
 * this narrows before handing off to cmdLaunch.
 */
const HARNESSES: readonly Harness[] = ['claude', 'codex'];

/**
 * Read a `LaunchInput` from the argument-shaped input object
 * (already JSON-parsed). Optional string/boolean fields pass through
 * as-is; unknown fields are ignored (forward-compat). Throws a
 * clear validation error naming the offending field on bad shape.
 */
function toLaunchInput(
  raw: unknown,
  env: NodeJS.ProcessEnv,
  coordRoot: string
): LaunchInput {
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) {
    throw new Error(
      'validation: input must be a JSON object'
    );
  }
  const r = raw as Record<string, unknown>;
  if (typeof r.harness !== 'string' || !HARNESSES.includes(r.harness as Harness)) {
    throw new Error(
      `validation: harness must be one of ${HARNESSES.join(', ')} (got ${JSON.stringify(r.harness)})`
    );
  }
  // Optional-string field guard.
  const optString = (key: string): string | undefined => {
    const v = r[key];
    if (v === undefined || v === null) return undefined;
    if (typeof v !== 'string') {
      throw new Error(
        `validation: ${key} must be a string (got ${typeof v})`
      );
    }
    return v;
  };
  // Optional-boolean field guard.
  const optBool = (key: string): boolean | undefined => {
    const v = r[key];
    if (v === undefined || v === null) return undefined;
    if (typeof v !== 'boolean') {
      throw new Error(
        `validation: ${key} must be a boolean (got ${typeof v})`
      );
    }
    return v;
  };
  const input: LaunchInput = {
    harness: r.harness as Harness,
    env,
    coordRoot,
  };
  // Optional strings.
  for (const key of [
    'identity',
    'model',
    'sessionName',
    'permissionMode',
    'persona',
    'agentBinary',
    'hooksDir',
    'stBinForHooks',
    'ptyBinPath',
    'cwd',
    'home',
  ] as const) {
    const v = optString(key);
    if (v !== undefined) {
      (input as unknown as Record<string, unknown>)[key] = v;
    }
  }
  // Optional booleans.
  for (const key of [
    'noPty',
    'noChannel',
    'noHooks',
    'ding',
    'fresh',
    'permanent',
    'unattended',
    'dryRun',
    'captureOnly',
  ] as const) {
    const v = optBool(key);
    if (v !== undefined) {
      (input as unknown as Record<string, unknown>)[key] = v;
    }
  }
  return input;
}

async function readAllStdin(ctx: CliContext): Promise<string> {
  const raw = await ctx.readStdin();
  if (raw instanceof Buffer) return raw.toString('utf8');
  return String(raw);
}

const LAUNCH_CORE_HELP =
  'usage: st __launch-core\n\n' +
  '  Hidden JSON-in/JSON-out entrypoint. Contract:\n' +
  '  - STDIN: JSON body of LaunchInput (minus env + coordRoot).\n' +
  '  - STDOUT: JSON body of LaunchResult on success.\n' +
  '  - STDERR: error message on non-zero exit.\n' +
  '  - EXIT: 0=ok, 1=validation-error, 2=internal-error.\n\n' +
  '  For convoy bridge use only. Do NOT depend on this from a\n' +
  '  user shell — the user-facing verb is `st launch` (retiring)\n' +
  '  or `convoy add` (canonical).\n';

/**
 * The CLI wrapper for `st __launch-core`. Reads JSON from stdin,
 * validates, delegates to {@link cmdLaunch}, writes LaunchResult
 * JSON to stdout, returns an exit code.
 */
export async function cmdLaunchCoreCli(
  args: readonly string[],
  ctx: CliContext
): Promise<number> {
  for (const a of args) {
    if (a === '-h' || a === '--help') {
      ctx.stderr(LAUNCH_CORE_HELP);
      return 0;
    }
    // No other flags accepted — this is a pure JSON-body entrypoint.
    ctx.stderr(`unknown argument: ${a}\n`);
    ctx.stderr(LAUNCH_CORE_HELP);
    return 1;
  }

  let bodyText: string;
  try {
    bodyText = await readAllStdin(ctx);
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    ctx.stderr(`internal: failed to read stdin: ${msg}\n`);
    return 2;
  }

  let raw: unknown;
  try {
    raw = JSON.parse(bodyText);
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    ctx.stderr(`validation: stdin is not valid JSON: ${msg}\n`);
    return 1;
  }

  let input: LaunchInput;
  try {
    input = toLaunchInput(raw, ctx.env, ctx.coordRoot);
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    ctx.stderr(`${msg}\n`);
    return 1;
  }

  let result: LaunchResult;
  try {
    result = await cmdLaunch(input, ctx);
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    ctx.stderr(`internal: launch failed: ${msg}\n`);
    return 2;
  }

  ctx.stdout(JSON.stringify(result) + '\n');
  return 0;
}
