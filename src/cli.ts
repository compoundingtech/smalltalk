// CLI dispatcher.
//
// Each src/commands/<name>.ts exports a `cmdXCli(args, ctx)` wrapper that
// parses argv, calls the typed core, and writes output via the
// {@link CliContext} sinks. This file is now essentially:
// (1) parse the top-level subcommand,
// (2) dispatch to subcommands,
// (3) dispatch to the right cmdXCli,
// (4) catch StError → stderr + exit 1.

import { spawnSync } from 'node:child_process';
import { readFileSync, realpathSync, statSync } from 'node:fs';
import { delimiter, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { stConfigFrom, stRootFrom } from './common.ts';
import {
  invokedName,
  StdinReadTimeoutError,
  type CliContext,
} from './cli-context.ts';
import { cmdArchiveCli } from './commands/archive.ts';
import { cmdCompletionsCli } from './commands/completions.ts';
import { cmdContextCli } from './commands/context.ts';
import { cmdDingCli } from './commands/ding.ts';
import { cmdHooksCli } from './commands/hooks.ts';
import { cmdInitCli } from './commands/init.ts';
import { cmdLsCli } from './commands/ls.ts';
import { cmdMcpCli } from './commands/mcp.ts';
import { cmdAgentsCli } from './commands/agents.ts';
import { cmdOverviewCli } from './commands/overview.ts';
import { cmdReadCli } from './commands/read.ts';
import { cmdResourceCli } from './commands/resource.ts';
import { cmdReplyCli } from './commands/reply.ts';
import { cmdSendCli } from './commands/send.ts';
import { cmdStatusCli } from './commands/status.ts';
import { cmdSyncCli } from './commands/sync.ts';
import { cmdThreadCli } from './commands/thread.ts';
import { cmdWatchCli } from './commands/watch.ts';

export type { CliContext } from './cli-context.ts';

export function defaultCliContext(): CliContext {
  return {
    env: process.env,
    stRoot: stRootFrom(process.env),
    stConfig: stConfigFrom(process.env),
    stdout: (s) => process.stdout.write(s),
    stderr: (s) => process.stderr.write(s),
    readStdin: (opts) => readStdinBuffer(process.stdin, opts),
    // brief-033: brief checks isTTY === true (a real TTY); anything
    // else (piped stdin, redirected file, non-tty subprocess pipe) is
    // treated as "stdin is connected to something" and the conflict
    // guard fires when paired with `-m`.
    stdinIsTty: () => process.stdin.isTTY === true,
  };
}

async function readStdinBuffer(
  stream: NodeJS.ReadableStream,
  opts: { timeoutMs?: number } = {}
): Promise<Buffer> {
  const readAll = async (): Promise<Buffer> => {
    const chunks: Buffer[] = [];
    for await (const chunk of stream) {
      chunks.push(
        typeof chunk === 'string' ? Buffer.from(chunk, 'utf8') : chunk
      );
    }
    return Buffer.concat(chunks);
  };

  const timeoutMs = opts.timeoutMs;
  if (timeoutMs === undefined || timeoutMs <= 0) {
    return readAll();
  }

  // Bounded read: race the full drain against a timer. If the stream
  // produces no EOF in time (an inherited pipe with no writer, or a TTY we
  // shouldn't have been reading), tear it down so the pending read settles
  // and the process can exit, then reject — never block forever.
  let timer: NodeJS.Timeout | undefined;
  const readPromise = readAll();
  // Swallow the late rejection the torn-down stream produces after the
  // timeout has already won the race (avoids an unhandledRejection).
  readPromise.catch(() => {});
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => {
      const s = stream as Partial<NodeJS.ReadStream>;
      // unref() so this handle stops holding the event loop open (the
      // dispatcher exits via `process.exitCode`, i.e. a natural drain);
      // destroy() settles the pending read. Without unref the process
      // would print the error but then hang on exit until killed.
      s.unref?.();
      s.destroy?.();
      reject(new StdinReadTimeoutError(timeoutMs));
    }, timeoutMs);
  });

  try {
    return await Promise.race([readPromise, timeout]);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
}

function messageUsage(name: string): string {
  return (
    `usage: ${name} message <verb> [args...]   (alias: ${name} msg <verb>)\n\n` +
    `  send <to> [-m <body> | --message <body>] [--from ID] [--subject S]\n` +
    `           [--in-reply-to F] [--tags T,T] [--priority P]\n` +
    `                                   read body from stdin if -m omitted\n` +
    `  reply <thread-filename> [-m <body> | --message <body>] [--subject S] [--from ID]\n` +
    `                                   recipient derived from thread's from:\n` +
    `  ls [<identity>] [--archive] [--count|--json] [--since UNIX_MS] [--from ID] [--orphans]\n` +
    `  read [<identity>] <filename> [--raw|--json] [--archive]\n` +
    `  archive [<identity>] <filename> [--with-attachments]\n` +
    `  archive trim [<identity>] [--older-than DURATION] [--keep-last N] [--dry-run]\n` +
    `                                   [--with-attachments]\n` +
    `  thread [<identity>] <filename> [--tree]\n\n` +
    `  Example: echo 'hi bob' | ${name} message send bob --subject hello\n` +
    `  Tip: run \`${name} message <verb> --help\` for a verb's flags + examples.\n`
  );
}

function topLevelUsage(name: string): string {
  return (
    `usage: ${name} <subcommand> [args...]\n` +
    `       ${name} --help | --version\n\n` +
  `Messages:\n` +
  `  message <verb> [args...]   (alias: msg) — send/reply/read/archive on the bus\n` +
  `    send | reply | ls | read | archive | thread\n\n` +
  `Live:\n` +
  `  watch [<identity>] [--all] [--with-subject] [--since UNIX_MS | --since-now]\n` +
  `                     [--interval MS] [--once]\n` +
  `                     follow inbox arrivals; --all is cross-tree\n` +
  `  status [<identity>] [--set <state>]            get or set an agent status\n` +
  `  agents [--status STATE] [--json [--enrich]]    who's around (alias: members)\n` +
  `  overview [--recent N] [--json]                 at-a-glance dashboard\n\n` +
  `Resources:  publish + list the URLs an agent cares about\n` +
  `  resource add <url> [--title T] [--tag T,T] [--body-stdin]\n` +
  `  resource ls [<identity>] [--json]\n` +
  `  resource read [<identity>] <filename> [--json]\n` +
  `  resource rm <filename>\n\n` +
  `Context (brief-024, lossless-restart):\n` +
  `  context read [<identity>] [--decisions | --full]\n` +
  `                                   print now.md / decisions/ log / both\n` +
  `  context write [<identity>]       replace now.md from stdin\n` +
  `  context append [<identity>] --decision "<text>" --why "<text>"\n` +
  `                                   append one entry to decisions/ as a new\n` +
  `                                   file named <unix-ms>-<rand6>.md\n\n` +
  `Sync:\n` +
  `  sync push <peer>\n` +
  `  sync push --all\n` +
  `  sync pull <peer>\n` +
  `  sync pull --all                  recommended cron default (pull-only)\n` +
  `  sync --all                       push + pull against every peer\n` +
  `  sync sweep                       enforce the LAYOUT tombstone invariant\n\n` +
  `Hooks:\n` +
  `  hooks path [--for FAMILY] [--json]\n` +
  `                                   print where st's agent-integration hook\n` +
  `                                   scripts live + the exact install config\n` +
  `                                   (claude-code|codex|pi). Read-only; prints,\n` +
  `                                   never installs. --json for tools\n\n` +
  `Embedding:\n` +
  `  mcp                              run as an MCP stdio server\n` +
  `  init [<dir>] [--no-channel] [--print] [--force]\n` +
  `                                   write or merge .mcp.json in <dir>\n` +
  `                                   (default: cwd) so Claude Code loads\n` +
  `                                   the smalltalk MCP server\n` +
  `  ding <pty-session> [--identity ID] [--interval MS]\n` +
  `                                   busy-aware push notifier; pty-sends a\n` +
  `                                   notice on each new arrival when the agent\n` +
  `                                   isn't busy/dnd\n` +
  `  completions <shell>              print a shell completion script to\n` +
  `                                   stdout (fish | bash | zsh), e.g.\n` +
  `                                   ${name} completions fish > \\\n` +
  `                                     ~/.config/fish/completions/${name}.fish\n\n` +
  `Run \`${name} message --help\` for the full message-verb flag surface.\n` +
    `See LAYOUT.md for the data-format spec.\n`
  );
}

/**
 * Set of OLD top-level subcommand names that are now nested under
 * `st message`. The dispatcher detects them and emits a helpful
 * "Did you mean st message <verb>?" pointer.
 */
const NESTED_MESSAGE_VERBS = new Set([
  'send',
  'ls',
  'read',
  'archive',
  'thread',
]);

const MESSAGE_GROUP_NAMES = new Set(['message', 'msg']);

async function dispatchMessage(
  args: readonly string[],
  ctx: CliContext
): Promise<number> {
  const name = invokedName(ctx.env);
  const sub = args[0];
  if (sub === undefined) {
    ctx.stderr(messageUsage(name));
    return 2;
  }
  if (sub === 'help' || sub === '--help' || sub === '-h') {
    ctx.stdout(messageUsage(name));
    return 0;
  }
  const rest = args.slice(1);
  switch (sub) {
    case 'send':
      return await cmdSendCli(rest, ctx);
    case 'reply':
      return await cmdReplyCli(rest, ctx);
    case 'ls':
      return cmdLsCli(rest, ctx);
    case 'read':
      return cmdReadCli(rest, ctx);
    case 'archive':
      return cmdArchiveCli(rest, ctx);
    case 'thread':
      return cmdThreadCli(rest, ctx);
    default:
      ctx.stderr(`${name} message: unknown subcommand: ${sub}\n\n`);
      ctx.stderr(messageUsage(name));
      return 2;
  }
}

/**
 * Short git SHA of the checkout this module lives in, e.g. `abc1234`,
 * or null when unavailable (not a git checkout, git not on PATH, a
 * tarball/npm install with no `.git`). Probes the MODULE's repo dir —
 * not `process.cwd()` — so the SHA identifies the `st` build itself,
 * regardless of which repo the user is standing in.
 */
function gitShortSha(repoDir: string): string | null {
  try {
    const r = spawnSync('git', ['rev-parse', '--short', 'HEAD'], {
      cwd: repoDir,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
    });
    if (r.status !== 0 || typeof r.stdout !== 'string') return null;
    const sha = r.stdout.trim();
    return sha.length > 0 ? sha : null;
  } catch {
    return null;
  }
}

/**
 * `<invokedName> <semver>+<short-sha>\n` — the payload for `<name>
 * --version`. Reads package.json at runtime relative to this module's
 * on-disk location, so it works under `npm link` (where the source
 * lives elsewhere on the filesystem) without a build-time constant.
 * Follows the same invoked-name convention as the help banners: `st
 * --version` prints e.g. `st 0.3.0+abc1234`. The `+<short-sha>` build
 * suffix is dropped gracefully when the SHA can't be resolved (not a
 * git checkout / no git), leaving a plain `st 0.3.0`.
 */
function versionString(env: NodeJS.ProcessEnv): string {
  const here = fileURLToPath(import.meta.url);
  const repoDir = join(dirname(here), '..');
  const pkgPath = join(repoDir, 'package.json');
  const pkg = JSON.parse(readFileSync(pkgPath, 'utf8')) as {
    version: string;
  };
  const sha = gitShortSha(repoDir);
  const version = sha !== null ? `${pkg.version}+${sha}` : pkg.version;
  return `${invokedName(env)} ${version}\n`;
}

export async function runCli(
  argv: readonly string[],
  ctx: CliContext = defaultCliContext()
): Promise<number> {
  const name = invokedName(ctx.env);
  if (argv.length === 0) {
    ctx.stderr(topLevelUsage(name));
    return 2;
  }
  const cmd = argv[0]!;
  if (cmd === 'help' || cmd === '--help' || cmd === '-h') {
    ctx.stdout(topLevelUsage(name));
    return 0;
  }
  if (cmd === '--version') {
    ctx.stdout(versionString(ctx.env));
    return 0;
  }
  const rest = argv.slice(1);
  try {
    if (MESSAGE_GROUP_NAMES.has(cmd)) {
      return await dispatchMessage(rest, ctx);
    }
    switch (cmd) {
      case 'watch':
        return await cmdWatchCli(rest, ctx);
      case 'status':
        return cmdStatusCli(rest, ctx);
      case 'agents':
        return cmdAgentsCli(rest, ctx);
      case 'overview':
        return cmdOverviewCli(rest, ctx);
      case 'sync':
        return await cmdSyncCli(rest, ctx);
      case 'hooks':
        return cmdHooksCli(rest, ctx);
      case 'resource':
        return await cmdResourceCli(rest, ctx);
      case 'context':
        return await cmdContextCli(rest, ctx);
      case 'mcp':
        return await cmdMcpCli(rest, ctx);
      case 'init':
        return await cmdInitCli(rest, ctx);
      case 'ding':
        return await cmdDingCli(rest, ctx);
      case 'completions':
        return cmdCompletionsCli(rest, ctx);
      default:
        // Helpful pointer for users who still type the pre-brief-017
        // flat forms: `st send` → `st message send`.
        if (NESTED_MESSAGE_VERBS.has(cmd)) {
          ctx.stderr(
            `${name}: unknown subcommand: ${cmd}. Did you mean \`${name} message ${cmd}\`?\n\n`
          );
          ctx.stderr(topLevelUsage(name));
          return 2;
        }
        // Git-style PATH dispatch: look up `st-<cmd>` (canonical)
        // then `smalltalk-<cmd>`. Built-in commands above always
        // win — only unknown verbs reach this branch.
        {
          const plugin = findPlugin(cmd, ctx.env);
          if (plugin !== null) {
            const r = spawnSync(plugin, rest, {
              stdio: 'inherit',
              env: ctx.env,
            });
            return r.status ?? 1;
          }
        }
        ctx.stderr(`${name}: unknown subcommand: ${cmd}\n\n`);
        ctx.stderr(topLevelUsage(name));
        return 2;
    }
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    ctx.stderr(`${name}: ${msg}\n`);
    return 1;
  }
}

/**
 * Locate a plugin script on PATH.
 *
 * Tries each prefix in order — `st-`, `smalltalk-` — and returns the
 * absolute path of the first match. Post-st-cutover the `st-`
 * prefix is no longer scanned. The match must be a regular file with
 * at least one of the user/group/other exec bits set. Per-bucket
 * short-circuit means we won't iterate the full PATH for prefixes
 * that don't match anywhere.
 *
 * Built-in commands are dispatched before this is called, so a verb
 * like `st-message` (if one existed) can't shadow the built-in
 * `st message` group.
 */
function findPlugin(
  cmd: string,
  env: NodeJS.ProcessEnv
): string | null {
  const path = env.PATH ?? '';
  if (path.length === 0) return null;
  const dirs = path.split(delimiter).filter((d) => d.length > 0);
  for (const prefix of ['st-', 'smalltalk-']) {
    const name = `${prefix}${cmd}`;
    for (const dir of dirs) {
      const candidate = join(dir, name);
      try {
        const st = statSync(candidate);
        // 0o111 = any-exec bit (user|group|other). On a real Unix-y
        // PATH, this is the right gate — the file is a runnable script
        // or binary. Skip non-regular files (directories, FIFOs).
        if (st.isFile() && (st.mode & 0o111) !== 0) return candidate;
      } catch {
        // not found in this dir
      }
    }
  }
  return null;
}

// Entry-point guard. Two paths to canonicalize because Node follows
// symlinks during module load (so `import.meta.url` is the realpath of
// this file) while `process.argv[1]` is whatever the shell shim passed
// in — under `npm link`, that's the global symlink path. Comparing
// canonicalized paths makes the guard fire under both direct and
// symlinked invocations.
function isMainModule(): boolean {
  const arg = process.argv[1];
  if (arg === undefined) return false;
  try {
    const here = realpathSync(fileURLToPath(import.meta.url));
    return here === realpathSync(arg);
  } catch {
    return false;
  }
}

if (isMainModule()) {
  runCli(process.argv.slice(2)).then(
    (code) => {
      process.exitCode = code;
    },
    (err) => {
      process.stderr.write(
        `${invokedName(process.env)}: internal error: ${String(err)}\n`
      );
      process.exitCode = 1;
    }
  );
}
