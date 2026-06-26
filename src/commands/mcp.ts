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
  envIdentityFrom,
  invokedAsFrom,
} from '../common.ts';

export async function cmdMcpCli(
  args: readonly string[],
  ctx: CliContext
): Promise<number> {
  let channel = false;
  for (const a of args) {
    if (a === '-h' || a === '--help') {
      ctx.stderr(
        'usage: coord mcp [--channel]\n\n' +
          '  Run the coord MCP server over stdio. Reads $COORD_IDENTITY\n' +
          '  from the environment (required — no default). $COORD_ROOT\n' +
          '  defaults to ~/.local/state/coord, same as every other coord\n' +
          '  verb. Intended to be invoked by an MCP host (Claude Code,\n' +
          '  Codex, Pi).\n\n' +
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
  // ST_IDENTITY > COORD_IDENTITY (with the one-time fallback warning)
  // and no default — must be set by the host (Claude Code, Codex, Pi)
  // so the server knows whose inbox to watch.
  const root = coordRootFrom(ctx.env);
  const identity = envIdentityFrom(ctx.env);
  if (!identity) {
    throw new Error('ST_IDENTITY (or COORD_IDENTITY) must be set for `mcp`');
  }

  // Lazy-import: the @modelcontextprotocol/sdk + zod dep cost is paid
  // only when `coord mcp` is actually invoked.
  const { createMcpServer } = await import('../mcp/index.ts');
  const { asIdentity } = await import('../types.ts');

  const serverName = canonicalServerName(invokedAsFrom(ctx.env));

  const handle = createMcpServer({
    root,
    identity: asIdentity(identity),
    channel,
    serverName,
  });

  await handle.run();
  await handle.close();
  return 0;
}
