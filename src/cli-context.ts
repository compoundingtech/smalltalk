// cli-context.ts — shared shape passed by the dispatcher to every cmdXCli.
//
// Pulled out of cli.ts so each src/commands/*.ts can import the type
// without creating a circular import back to the dispatcher.

export interface CliContext {
  env: NodeJS.ProcessEnv;
  stRoot: string;
  stConfig: string;
  stdout: (s: string) => void;
  stderr: (s: string) => void;
  /**
   * Read all of stdin as a Buffer. CLI uses process.stdin; tests pass any.
   * Pass `{ timeoutMs }` to bound the read: if stdin produces no EOF
   * within the window (an inherited pipe with no writer), the underlying
   * stream is torn down and the promise rejects with
   * {@link StdinReadTimeoutError} instead of blocking forever.
   */
  readStdin: (opts?: { timeoutMs?: number }) => Promise<Buffer>;
  /**
   * brief-033: whether stdin is connected to a TTY (i.e., the user is
   * typing interactively, no piped input). Used by `st message
   * send` to distinguish "user passed -m and also piped stdin" (an
   * error) from "user passed -m, no pipe" (the happy inline path).
   * Defaults via the dispatcher to `() => process.stdin.isTTY === true`.
   * Optional so existing tests don't have to add it; when omitted,
   * callers should assume "not a TTY" (the safer default — pretend
   * stdin is piped — but tests that don't need the distinction won't
   * exercise the relevant branch).
   */
  stdinIsTty?: () => boolean;
}

/**
 * The name the user typed to invoke this CLI — `st`, `smalltalk`, or
 * `st`. Set by the bin/ shims via `_ST_INVOKED_AS` before exec'ing
 * into node. Help banners and error prefixes read this so users see
 * the name they typed, not a hard-coded value that goes stale as the
 * rename phases finish.
 *
 * Falls back to `st` when unset (fresh dev shells, unit tests, direct
 * `node src/cli.ts` invocations).
 */
export function invokedName(env: NodeJS.ProcessEnv): string {
  const raw = env._ST_INVOKED_AS;
  if (raw === undefined || raw.length === 0) return 'st';
  return raw;
}

/**
 * Thrown by the stdin reader when a bounded read (`readStdin({ timeoutMs })`)
 * elapses without reaching EOF — i.e. stdin was connected to something that
 * never closed (a TTY, or an inherited pipe with no writer). The message
 * verbs turn this into a clear "no message body" error instead of hanging.
 */
export class StdinReadTimeoutError extends Error {
  readonly timeoutMs: number;
  constructor(timeoutMs: number) {
    super(`timed out reading stdin after ${timeoutMs}ms`);
    this.name = 'StdinReadTimeoutError';
    this.timeoutMs = timeoutMs;
  }
}

/**
 * Default backstop (ms) for reading a message body from stdin. A real pipe
 * (echo/redirect/`$(...)`/command pipe) reaches EOF in milliseconds; this
 * bound only bites a stdin that never EOFs — the footgun that hangs a
 * detached agent forever. Override with `$ST_STDIN_TIMEOUT_MS`.
 */
export const STDIN_BODY_TIMEOUT_MS = 10_000;

/** Resolve the stdin-read timeout, honoring `$ST_STDIN_TIMEOUT_MS`. */
export function stdinBodyTimeoutMs(env: NodeJS.ProcessEnv): number {
  const raw = env.ST_STDIN_TIMEOUT_MS;
  if (raw !== undefined && raw.length > 0) {
    const n = Number(raw);
    if (Number.isFinite(n) && n > 0) return n;
  }
  return STDIN_BODY_TIMEOUT_MS;
}

/**
 * Resolve a message body for `send` / `reply` WITHOUT ever blocking forever
 * on stdin. Precedence (per the st-send stdin-footgun fix):
 *
 *   1. An inline `-m <body>` wins outright — stdin is NEVER touched. This is
 *      the safe path for automated callers (agents), which usually run with
 *      an inherited, never-closing stdin.
 *   2. No inline body + interactive TTY stdin → there is no body coming, so
 *      throw a clear error instead of blocking on a read that never EOFs.
 *   3. No inline body + piped/redirected stdin → read it, but with a timeout
 *      backstop so a pipe that never closes errors clearly instead of hanging.
 */
export async function resolveMessageBody(
  ctx: CliContext,
  inlineBody: string | undefined
): Promise<string | Buffer> {
  // (1) Inline body wins — never read stdin.
  if (inlineBody !== undefined) return inlineBody;

  // (2) A TTY means the user is at a prompt with nothing piped; reading
  // would block until they hit Ctrl-D. Refuse loudly instead.
  const isTty = ctx.stdinIsTty?.() ?? false;
  if (isTty) {
    throw new Error(
      'no message body — pass `-m <body>` or pipe the body via stdin ' +
        '(e.g. `echo hi | st message send bob`)'
    );
  }

  // (3) Piped/redirected stdin: read it, bounded, so a never-closing pipe
  // errors instead of hanging.
  try {
    return await ctx.readStdin({ timeoutMs: stdinBodyTimeoutMs(ctx.env) });
  } catch (err) {
    if (err instanceof StdinReadTimeoutError) {
      throw new Error(
        'no message body — timed out reading stdin (it was connected but ' +
          'sent no data and never closed). Pass `-m <body>`, or pipe a body ' +
          'that reaches EOF. Tune the window with $ST_STDIN_TIMEOUT_MS.'
      );
    }
    throw err;
  }
}
