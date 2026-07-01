// commands/mcp.ts — `coord mcp` CLI wrapper.
//
// Lazy-imports the heavy `@modelcontextprotocol/sdk` deps inside the
// function body so `coord send` / `ls` / `read` etc. don't pay the
// startup cost. Every MCP-related symbol is reachable only via this
// command's call path.

import type { CliContext } from '../cli-context.ts';
import {
  canonicalServerName,
  coordRootFrom,
  ensureIdentityDirs,
  envAgentFrom,
  invokedAsFrom,
  rand6,
} from '../common.ts';

/** Prefix that marks an MCP-fallback throwaway identity. Stable so
 *  operators / `st agents` listings can spot them at a glance. */
const ANON_PREFIX = 'anon-';

/** Build a throwaway agent name when the host spawned `st mcp` without
 *  setting `ST_AGENT`. Uses the same 6-char Crockford-base32 entropy
 *  as message filenames — same grammar guarantees, ~10⁹ namespace.
 *  Shape: `anon-abcd12` (11 chars, valid agent grammar). */
function generateAnonAgent(): string {
  return `${ANON_PREFIX}${rand6()}`;
}

export async function cmdMcpCli(
  args: readonly string[],
  ctx: CliContext
): Promise<number> {
  let channel = false;
  for (const a of args) {
    if (a === '-h' || a === '--help') {
      ctx.stderr(
        'usage: coord mcp [--channel]\n\n' +
          '  Run the coord MCP server over stdio. Reads $ST_AGENT (or\n' +
          '  legacy $ST_IDENTITY / $COORD_IDENTITY) from the environment.\n' +
          '  If none are set, falls back to a throwaway `anon-<rand6>`\n' +
          '  identity so the server still starts under MCP hosts that\n' +
          '  spawn `coord mcp` without identity env (Codex, etc.). The\n' +
          '  fallback is loud — one stderr line names the throwaway id\n' +
          '  and points at ST_AGENT for persistence. $ST_ROOT (or\n' +
          '  $COORD_ROOT) defaults to ~/.local/state/smalltalk, same as\n' +
          '  every other coord verb. Intended to be invoked by an MCP\n' +
          '  host (Claude Code, Codex, Pi).\n\n' +
          '  --channel   Enable Claude Code channel mode: advertise the\n' +
          '              experimental.claude/channel capability, watch the\n' +
          '              inbox for new files, and register the coord_msg_reply\n' +
          '              tool. Off by default; existing pull-only hosts are\n' +
          '              unaffected.\n'
      );
      return 0;
    }
    if (a === '--channel') {
      channel = true;
      continue;
    }
    throw new Error(`unknown flag: ${a}`);
  }

  // `st mcp` / `coord mcp` follows the env contract every other verb
  // uses: ST_ROOT > COORD_ROOT > default state path via coordRootFrom();
  // ST_AGENT > ST_IDENTITY (warn) > COORD_IDENTITY (warn).
  //
  // When none of those are set, the previous behavior was a hard exit.
  // That broke any MCP host that spawned `coord mcp` without an env
  // identity (Codex hit this in poc-server). Per Nathan's call, we
  // now fall back to a throwaway `anon-<rand6>` identity with a
  // single stderr warning — "so we don't just not work." Managed
  // hosts that DO set an identity are unaffected.
  const root = coordRootFrom(ctx.env);
  let identity = envAgentFrom(ctx.env);
  if (!identity) {
    identity = generateAnonAgent();
    // Lazy-create the agent's inbox/archive so the channel watcher
    // and status writer have something to point at. envAgentFrom's
    // resolveAgent flow normally does this for env-set identities;
    // we have to do it explicitly here because we generated the
    // identity ourselves and bypassed that flow.
    ensureIdentityDirs(identity, root);
    ctx.stderr(
      `[smalltalk] no ST_AGENT set; using throwaway identity ${identity} ` +
        `(set ST_AGENT to persist — sessions reusing the same identity ` +
        `share inbox/archive)\n`
    );
  }

  // Lazy-import: the @modelcontextprotocol/sdk + zod dep cost is paid
  // only when `coord mcp` is actually invoked.
  const { createMcpServer } = await import('../mcp/index.ts');
  const { asAgent } = await import('../types.ts');

  const serverName = canonicalServerName(invokedAsFrom(ctx.env));

  const handle = createMcpServer({
    root,
    identity: asAgent(identity),
    channel,
    serverName,
  });

  await handle.run();
  await handle.close();
  return 0;
}
