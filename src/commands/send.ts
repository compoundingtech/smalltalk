// commands/send.ts — write a new message directly to <to>/inbox/.
//
// Mirror of lib/cmd_send.sh. Per LAYOUT.md "Sending": the act of writing
// a uniquely-named markdown file into <recipient>/inbox/ IS the send.
// There is no outbox.
//
// This file exposes two symmetric entry points:
//
// - `cmdSend` (alias `cmdSendCore`): the pure, typed core. Takes a
//   {@link SendInput} object, throws CoordError subclasses, returns
//   {@link SendResult}. Used directly by createCoord and the unit tests.
//
// - `cmdSendCli`: the CLI wrapper. Parses argv, reads stdin, calls the
//   core, formats output via the {@link CliContext} sinks. Used by the
//   dispatcher in src/cli.ts.

import { mkdirSync } from 'node:fs';
import { join } from 'node:path';

import {
  emitFrontmatter,
  genFilename,
  inboxDir,
  resolveIdentity,
  safeAtomicWrite,
  validFilename,
  validIdentity,
} from '../common.ts';
import {
  EmptyBodyError,
  InvalidFilenameError,
  InvalidIdentityError,
  InvalidPriorityError,
} from '../errors.ts';

export interface SendInput {
  /** Recipient identity (positional `<to>`). Required. */
  to: string;
  /** Sender identity. Falls back to env COORD_IDENTITY. */
  from?: string | undefined;
  /** Optional subject. Will be YAML-quoted. */
  subject?: string | undefined;
  /** Optional in-reply-to filename. Validated against the LAYOUT grammar. */
  inReplyTo?: string | undefined;
  /**
   * Optional tags. Pass either a comma-separated string (CLI form) or an
   * already-split array. Whitespace around tags is trimmed; empties dropped.
   */
  tags?: string | string[] | undefined;
  /** Optional priority. */
  priority?: string | undefined;
  /** Message body (read from stdin in the CLI; passed directly in tests). */
  body: string | Buffer;

  env: NodeJS.ProcessEnv;
  coordRoot: string;
}

export interface SendResult {
  /** The generated filename, e.g. `1714826789012-x9k4mz.md`. */
  filename: string;
  /** Absolute path to the written file. */
  path: string;
}

const PRIORITIES = new Set(['low', 'normal', 'high']);
const COLLISION_ATTEMPTS = 5;

export function cmdSend(input: SendInput): SendResult {
  if (!input.to) throw new Error('<to> is required');
  if (!validIdentity(input.to)) {
    throw new InvalidIdentityError(input.to);
  }

  const from = resolveIdentity({
    explicit: input.from,
    env: input.env,
    coordRoot: input.coordRoot,
  });

  if (input.inReplyTo !== undefined && input.inReplyTo !== '') {
    if (!validFilename(input.inReplyTo)) {
      throw new InvalidFilenameError(input.inReplyTo);
    }
  }

  if (input.priority !== undefined && input.priority !== '') {
    if (!PRIORITIES.has(input.priority)) {
      throw new InvalidPriorityError(input.priority);
    }
  }

  const bodyBuf =
    typeof input.body === 'string' ? Buffer.from(input.body, 'utf8') : input.body;
  if (bodyBuf.length === 0) {
    throw new EmptyBodyError();
  }

  // Build the frontmatter map (only `from` is required by LAYOUT-004).
  const fm: Record<string, unknown> = { from };
  if (input.subject !== undefined && input.subject !== '') {
    fm.subject = input.subject;
  }
  if (input.inReplyTo !== undefined && input.inReplyTo !== '') {
    fm['in-reply-to'] = input.inReplyTo;
  }
  const tags = normalizeTags(input.tags);
  if (tags.length > 0) {
    fm.tags = tags;
  }
  if (input.priority !== undefined && input.priority !== '') {
    fm.priority = input.priority;
  }

  const inbox = inboxDir(input.to, input.coordRoot);
  mkdirSync(inbox, { recursive: true });

  // Compose the file body. Frontmatter + body, with a trailing newline if
  // the body doesn't already end in one (matches the bash impl).
  const trailingNl = bodyBuf.length > 0 && bodyBuf[bodyBuf.length - 1] !== 0x0a;
  const head = Buffer.from(emitFrontmatter(fm, ''), 'utf8');
  const tail = trailingNl ? Buffer.from('\n', 'utf8') : Buffer.alloc(0);
  const fileBuf = Buffer.concat([head, bodyBuf, tail]);

  // Try up to COLLISION_ATTEMPTS times: rand6 collisions in the same ms
  // are vanishingly rare, but we retry to surface the case loudly if the
  // clock is somehow stuck.
  let lastErr: unknown;
  for (let attempt = 0; attempt < COLLISION_ATTEMPTS; attempt++) {
    const filename = genFilename();
    const dest = join(inbox, filename);
    try {
      safeAtomicWrite(dest, fileBuf);
      return { filename, path: dest };
    } catch (err: unknown) {
      lastErr = err;
      const code = (err as NodeJS.ErrnoException).code;
      if (code !== 'EEXIST') {
        throw err;
      }
      // EEXIST: try a new rand6.
    }
  }
  throw new Error(
    `filename collisions exhausted (${COLLISION_ATTEMPTS} attempts) — clock drift?`
  );
}

function normalizeTags(input: string | string[] | undefined): string[] {
  if (input === undefined) return [];
  const raw = Array.isArray(input) ? input : input.split(',');
  return raw.map((t) => t.trim()).filter((t) => t.length > 0);
}

// Alias for the embeddable API. Keeping the original `cmdSend` name keeps
// existing unit tests + lib.ts callers stable; the `Core` name is the
// brief-008 canonical handle for the pure-typed entry point.
export { cmdSend as cmdSendCore };

// ─── CLI wrapper ────────────────────────────────────────────────────────

import { invokedName, type CliContext } from '../cli-context.ts';

export async function cmdSendCli(
  args: readonly string[],
  ctx: CliContext
): Promise<number> {
  let to: string | undefined;
  let from: string | undefined;
  let subject: string | undefined;
  let inReplyTo: string | undefined;
  let tags: string | undefined;
  let priority: string | undefined;
  // brief-033: `-m <body>` / `--message <body>` inline body alias for
  // the `git commit -m` ergonomic. Mutually exclusive with stdin.
  let inlineBody: string | undefined;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    switch (a) {
      case '--from':
        from = args[++i];
        break;
      case '--subject':
        subject = args[++i];
        break;
      case '--in-reply-to':
        inReplyTo = args[++i];
        break;
      case '--tags':
        tags = args[++i];
        break;
      case '--priority':
        priority = args[++i];
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
      case '--help':
        {
          const name = invokedName(ctx.env);
          ctx.stderr(
            `usage: ${name} message send <to> [-m <body> | --message <body>]\n` +
              '                            [--from ID] [--subject S] [--in-reply-to F]\n' +
              '                            [--tags T,T,...] [--priority low|normal|high]\n\n' +
              '  Body source: pass `-m <body>` for inline, or omit and pipe the body\n' +
              `  via stdin (e.g. \`echo hi | ${name} message send bob\`). Don't do both.\n`
          );
        }
        return 0;
      default:
        if (a.startsWith('-')) throw new Error(`unknown flag: ${a}`);
        if (to === undefined) to = a;
        else throw new Error(`unexpected arg: ${a}`);
    }
  }
  if (to === undefined) throw new Error('<to> is required');

  let body: string | Buffer;
  if (inlineBody !== undefined) {
    // -m given. If stdin is also connected (piped, redirected),
    // surface the conflict loudly rather than silently dropping
    // either source. When stdin is a TTY (interactive run) we skip
    // reading entirely — otherwise readStdin would block waiting
    // for EOF.
    const isTty = ctx.stdinIsTty?.() ?? false;
    if (!isTty) {
      const piped = await ctx.readStdin();
      if (piped.length > 0) {
        throw new Error(
          'specify body via -m OR stdin, not both'
        );
      }
    }
    body = inlineBody;
  } else {
    body = await ctx.readStdin();
  }

  const r = cmdSend({
    to,
    ...(from !== undefined && { from }),
    ...(subject !== undefined && { subject }),
    ...(inReplyTo !== undefined && { inReplyTo }),
    ...(tags !== undefined && { tags }),
    ...(priority !== undefined && { priority }),
    body,
    env: ctx.env,
    coordRoot: ctx.coordRoot,
  });
  ctx.stdout(`${r.filename}\n`);
  return 0;
}
