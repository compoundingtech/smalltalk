// cli-context.ts — shared shape passed by the dispatcher to every cmdXCli.
//
// Pulled out of cli.ts so each src/commands/*.ts can import the type
// without creating a circular import back to the dispatcher.

export interface CliContext {
  env: NodeJS.ProcessEnv;
  coordRoot: string;
  coordConfig: string;
  stdout: (s: string) => void;
  stderr: (s: string) => void;
  /** Read all of stdin as a Buffer. CLI uses process.stdin; tests pass any. */
  readStdin: () => Promise<Buffer>;
  /**
   * brief-033: whether stdin is connected to a TTY (i.e., the user is
   * typing interactively, no piped input). Used by `coord message
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
 * `coord`. Set by the bin/ shims via `_ST_INVOKED_AS` before exec'ing
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
