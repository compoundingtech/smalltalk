// CLI dispatcher.
//
// Each src/commands/<name>.ts exports a `cmdXCli(args, ctx)` wrapper that
// parses argv, calls the typed core, and writes output via the
// {@link CliContext} sinks. This file is now essentially:
// (1) parse the top-level subcommand,
// (2) dispatch to subcommands,
// (3) dispatch to the right cmdXCli,
// (4) catch CoordError → stderr + exit 1.

import { realpathSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { coordConfigFrom, coordRootFrom } from './common.ts';
import type { CliContext } from './cli-context.ts';
import { cmdArchiveCli } from './commands/archive.ts';
import { cmdCompletionsCli } from './commands/completions.ts';
import { cmdDingCli } from './commands/ding.ts';
import { cmdInitCli } from './commands/init.ts';
import { cmdJournalCli } from './commands/journal.ts';
import { cmdLsCli } from './commands/ls.ts';
import { cmdMcpCli } from './commands/mcp.ts';
import { cmdMembersCli } from './commands/members.ts';
import { cmdOverviewCli } from './commands/overview.ts';
import { cmdReadCli } from './commands/read.ts';
import { cmdSendCli } from './commands/send.ts';
import { cmdStatusCli } from './commands/status.ts';
import { cmdSyncCli } from './commands/sync.ts';
import { cmdTaskCli } from './commands/task.ts';
import { cmdTasksCli } from './commands/tasks.ts';
import { cmdThreadCli } from './commands/thread.ts';
import { cmdWatchCli } from './commands/watch.ts';

export type { CliContext } from './cli-context.ts';

export function defaultCliContext(): CliContext {
  return {
    env: process.env,
    coordRoot: coordRootFrom(process.env),
    coordConfig: coordConfigFrom(process.env),
    stdout: (s) => process.stdout.write(s),
    stderr: (s) => process.stderr.write(s),
    readStdin: () => readStdinBuffer(process.stdin),
    // brief-033: brief checks isTTY === true (a real TTY); anything
    // else (piped stdin, redirected file, non-tty subprocess pipe) is
    // treated as "stdin is connected to something" and the conflict
    // guard fires when paired with `-m`.
    stdinIsTty: () => process.stdin.isTTY === true,
  };
}

async function readStdinBuffer(
  stream: NodeJS.ReadableStream
): Promise<Buffer> {
  const chunks: Buffer[] = [];
  for await (const chunk of stream) {
    chunks.push(typeof chunk === 'string' ? Buffer.from(chunk, 'utf8') : chunk);
  }
  return Buffer.concat(chunks);
}

const MESSAGE_USAGE =
  `usage: coord message <verb> [args...]   (alias: coord msg <verb>)\n\n` +
  `  send <to> [--from ID] [--subject S] [--in-reply-to F] [--tags T,T] [--priority P]\n` +
  `                                   read body from stdin\n` +
  `  ls [<identity>] [--archive] [--count|--json] [--since UNIX_MS] [--from ID]\n` +
  `  read [<identity>] <filename> [--raw] [--archive]\n` +
  `  archive [<identity>] <filename>\n` +
  `  archive trim [<identity>] [--older-than DURATION] [--keep-last N] [--dry-run]\n` +
  `  thread [<identity>] <filename> [--tree]\n`;

const TOP_LEVEL_USAGE =
  `usage: coord <subcommand> [args...]\n\n` +
  `Messages:\n` +
  `  message <verb> [args...]   (alias: msg)\n` +
  `    send | ls | read | archive | thread\n\n` +
  `Live:\n` +
  `  watch [<identity>] [--all] [--with-subject] [--since UNIX_MS | --since-now]\n` +
  `                     [--interval MS] [--once]\n` +
  `                     default: watch your own inbox; --all is cross-tree\n` +
  `  status [<identity>] [--set <state>]\n` +
  `  members [--status STATE] [--json [--enrich]]\n` +
  `  overview [--recent N] [--json]\n\n` +
  `Tasks:\n` +
  `  task new "<title>" [--priority P] [--tag T,T] [--due Y-M-D]\n` +
  `                     [--from-message <inbox-filename>] [--no-edit]\n` +
  `  task ls [--status STATE] [--tag T] [--priority P] [--json]\n` +
  `  task status <filename> <STATE>\n` +
  `  task done <filename>\n` +
  `  task edit <filename>\n` +
  `  tasks [<identity>] [--status STATE] [--tag T] [--priority P]\n` +
  `                     [--json [--include-body]] [--watch [--interval MS]]\n\n` +
  `Journal:\n` +
  `  journal new "<body>" [--slug ...] [--topic ...] [--tag T,T]\n` +
  `              [--stdin | --edit]\n` +
  `  journal ls [<identity>] [--since <unix-ms>]\n` +
  `  journal cat [<identity>] <filename>\n` +
  `  journal tail [<identity>] [-n N]\n\n` +
  `Sync:\n` +
  `  sync push <peer>\n` +
  `  sync push --all\n` +
  `  sync pull <peer>\n` +
  `  sync pull --all                  recommended cron default (pull-only)\n` +
  `  sync --all                       push + pull against every peer\n` +
  `  sync sweep                       enforce the LAYOUT tombstone invariant\n\n` +
  `Embedding:\n` +
  `  mcp                              run as an MCP stdio server\n` +
  `  init [<dir>] [--no-channel] [--print] [--force]\n` +
  `                                   write or merge .mcp.json in <dir>\n` +
  `                                   (default: cwd) so Claude Code loads\n` +
  `                                   the coord MCP server\n` +
  `  ding <pty-session> [--identity ID] [--interval MS]\n` +
  `                                   busy-aware push notifier; pty-sends a\n` +
  `                                   notice on each new arrival when the agent\n` +
  `                                   isn't busy/dnd\n` +
  `  completions <shell>              print a shell completion script to\n` +
  `                                   stdout (fish | bash | zsh), e.g.\n` +
  `                                   coord completions fish > \\\n` +
  `                                     ~/.config/fish/completions/coord.fish\n\n` +
  `Run \`coord message --help\` for the full message-verb flag surface.\n` +
  `See LAYOUT.md for the data-format spec.\n`;

/**
 * Set of OLD top-level subcommand names that are now nested under
 * `coord message`. The dispatcher detects them and emits a helpful
 * "Did you mean coord message <verb>?" pointer.
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
  const sub = args[0];
  if (sub === undefined) {
    ctx.stderr(MESSAGE_USAGE);
    return 2;
  }
  if (sub === 'help' || sub === '--help' || sub === '-h') {
    ctx.stdout(MESSAGE_USAGE);
    return 0;
  }
  const rest = args.slice(1);
  switch (sub) {
    case 'send':
      return await cmdSendCli(rest, ctx);
    case 'ls':
      return cmdLsCli(rest, ctx);
    case 'read':
      return cmdReadCli(rest, ctx);
    case 'archive':
      return cmdArchiveCli(rest, ctx);
    case 'thread':
      return cmdThreadCli(rest, ctx);
    default:
      ctx.stderr(`coord message: unknown subcommand: ${sub}\n\n`);
      ctx.stderr(MESSAGE_USAGE);
      return 2;
  }
}

export async function runCli(
  argv: readonly string[],
  ctx: CliContext = defaultCliContext()
): Promise<number> {
  if (argv.length === 0) {
    ctx.stderr(TOP_LEVEL_USAGE);
    return 2;
  }
  const cmd = argv[0]!;
  if (cmd === 'help' || cmd === '--help' || cmd === '-h') {
    ctx.stdout(TOP_LEVEL_USAGE);
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
      case 'members':
        return cmdMembersCli(rest, ctx);
      case 'overview':
        return cmdOverviewCli(rest, ctx);
      case 'sync':
        return cmdSyncCli(rest, ctx);
      case 'task':
        return await cmdTaskCli(rest, ctx);
      case 'tasks':
        return await cmdTasksCli(rest, ctx);
      case 'journal':
        return await cmdJournalCli(rest, ctx);
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
        // flat forms: `coord send` → `coord message send`.
        if (NESTED_MESSAGE_VERBS.has(cmd)) {
          ctx.stderr(
            `coord: unknown subcommand: ${cmd}. Did you mean \`coord message ${cmd}\`?\n\n`
          );
        } else {
          ctx.stderr(`coord: unknown subcommand: ${cmd}\n\n`);
        }
        ctx.stderr(TOP_LEVEL_USAGE);
        return 2;
    }
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    ctx.stderr(`coord: ${msg}\n`);
    return 1;
  }
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
      process.stderr.write(`coord: internal error: ${String(err)}\n`);
      process.exitCode = 1;
    }
  );
}
