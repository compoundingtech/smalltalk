// commands/reply.ts — `st message reply <thread> [-m <body>] [--subject S]`
//
// Locates <thread> on the local tree, derives the recipient from the
// thread's `from:` frontmatter, and writes a reply into that
// recipient's inbox with `inReplyTo: <thread>` set. Same locate +
// derive semantics as the MCP `st_msg_reply` tool (both share
// `src/locate-thread.ts`).
//
// Was missing from the CLI dispatcher through the ding-mode work:
// DING-BUS.md instructs every ding-mode agent to run `st message
// reply <filename> -m "<body>"` on inbox arrivals, but the CLI
// switch at `src/cli.ts:dispatchMessage` didn't route `reply`. Every
// ding-mode agent errored with `unknown subcommand: reply` on their
// first response. This module wires the verb.

import { envAgentFrom } from '../common.ts';
import { invokedName } from '../cli-context.ts';
import { locateThread } from '../locate-thread.ts';
import { asFilename, asIdentity } from '../types.ts';

import type { CliContext } from '../cli-context.ts';

import { cmdSend } from './send.ts';

export interface ReplyInput {
  /** Filename of the message being replied to (LAYOUT grammar). */
  thread: string;
  /** Reply body — inline text OR raw bytes read from stdin. */
  body: string | Buffer;
  /** Optional subject override. Default: `re: <original-subject>`
   *  when the thread had a subject, else omitted. */
  subject?: string | undefined;
  /** Optional sender override. Default: `$ST_AGENT` (or legacy
   *  fallbacks). Same resolution as `st message send`. */
  from?: string | undefined;
  env: NodeJS.ProcessEnv;
  coordRoot: string;
}

export interface ReplyResult {
  filename: string;
  identity: string;
}

/**
 * Programmatic entry — the shape embedders + tests use. CLI dispatch
 * goes through `cmdReplyCli` which handles stdin + flag parsing then
 * delegates here.
 */
export function cmdReply(input: ReplyInput): ReplyResult {
  const selfId = envAgentFrom(input.env);
  if (selfId === undefined || selfId === '') {
    throw new Error(
      '<self identity> required — set $ST_AGENT or pass --from <ID>'
    );
  }
  const thread = asFilename(input.thread);
  const located = locateThread(
    input.coordRoot,
    asIdentity(selfId),
    thread
  );
  const recipient = located.from;
  const subject =
    input.subject !== undefined
      ? input.subject
      : located.subject !== undefined
        ? `re: ${located.subject}`
        : undefined;
  const r = cmdSend({
    to: recipient,
    ...(input.from !== undefined && { from: input.from }),
    ...(subject !== undefined && { subject }),
    inReplyTo: thread,
    body: input.body,
    env: input.env,
    coordRoot: input.coordRoot,
  });
  return { filename: r.filename, identity: recipient };
}

export async function cmdReplyCli(
  args: readonly string[],
  ctx: CliContext
): Promise<number> {
  let thread: string | undefined;
  let subject: string | undefined;
  let from: string | undefined;
  let inlineBody: string | undefined;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '--subject':
        subject = args[++i];
        break;
      case '--from':
        from = args[++i];
        break;
      case '-m':
      case '--message': {
        const v = args[++i];
        if (v === undefined) {
          throw new Error(`${a} requires a value`);
        }
        inlineBody = v;
        break;
      }
      case '-h':
      case '--help': {
        const name = invokedName(ctx.env);
        ctx.stderr(
          `usage: ${name} message reply <thread-filename> [-m <body> | --message <body>]\n` +
            '                                     [--subject S] [--from ID]\n\n' +
            '  Reply to <thread-filename>. Recipient is derived from the thread\'s\n' +
            '  `from:` field; default subject is `re: <original-subject>` (or omitted\n' +
            '  if the original had no subject). Body source: pass `-m <body>` for\n' +
            `  inline, or omit and pipe the body via stdin (e.g. \`echo ok | ${name} message reply <fn>\`).\n` +
            '  Don\'t do both.\n'
        );
        return 0;
      }
      default:
        if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
        if (thread === undefined) thread = a;
        else throw new Error(`unexpected arg: ${a}`);
    }
  }
  if (thread === undefined) {
    throw new Error('<thread-filename> is required');
  }

  let body: string | Buffer;
  if (inlineBody !== undefined) {
    // -m given. Guard the same "don't accept both inline + stdin"
    // rule as `st message send`: silent-drop of either source is
    // worse than a loud error.
    const isTty = ctx.stdinIsTty?.() ?? false;
    if (!isTty) {
      const piped = await ctx.readStdin();
      if (piped.length > 0) {
        throw new Error('specify body via -m OR stdin, not both');
      }
    }
    body = inlineBody;
  } else {
    body = await ctx.readStdin();
  }

  const r = cmdReply({
    thread,
    body,
    ...(subject !== undefined && { subject }),
    ...(from !== undefined && { from }),
    env: ctx.env,
    coordRoot: ctx.coordRoot,
  });
  ctx.stdout(`${r.filename}\n`);
  return 0;
}
