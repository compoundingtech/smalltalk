// mcp/tools/agents.ts — registers the `coord_agents` MCP tool plus its
// deprecated `coord_members` alias (and both `st_*` counterparts via
// the dual-prefix pattern).
//
// Thin wrapper over `cmdAgents` from commands/agents.ts. The CLI entry
// point (cmdAgentsCli) and this tool share the same pure enumeration;
// no shelling out.
//
// brief-009 item 3 (rename): `members` was renamed to `agents`. The
// old tool name is kept registered alongside the new one with the SAME
// handler, so calls in flight from agents that haven't migrated keep
// working. The output payload still uses the field name `members:` in
// the structured content — that's a wire shape kept for back-compat;
// will rename to `agents:` in a follow-up.

import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { z } from 'zod';

import { cmdAgents } from '../../commands/agents.ts';
import type { Coord } from '../../lib.ts';
import { STATES } from '../../types.ts';
import {
  buildToolResult,
  withErrorMapping,
} from '../error-mapping.ts';
import { registerDualTool } from './dual-register.ts';

const agentsInputShape = {
  status: z
    .enum(STATES)
    .optional()
    .describe(
      "Filter to agents whose effective status matches. `unknown` is the derived state when a peer's status-file mtime is older than ~15 minutes. Default: all."
    ),
  enrich: z
    .boolean()
    .optional()
    .describe(
      'Include lastActivity and inbox unread count per agent.'
    ),
};

const agentShape = {
  identity: z.string(),
  status: z.enum(STATES),
  name: z.string().nullable(),
  lastActivity: z
    .number()
    .nullable()
    .optional()
    .describe('Newest mtime under <agent>/ across inbox/archive/status. Enriched only.'),
  inbox: z
    .number()
    .int()
    .nonnegative()
    .optional()
    .describe('Count of valid-grammar files in <agent>/inbox/. Enriched only.'),
};

const agentsOutputShape = {
  members: z
    .array(z.object(agentShape))
    .describe('Agents under $ST_ROOT, sorted alphabetically. (Field name `members` kept for back-compat; renames to `agents` in a follow-up.)'),
};

export function registerAgentsTool(mcp: McpServer, coord: Coord): void {
  const handler = async (args: { status?: string; enrich?: boolean }) =>
    withErrorMapping(async () => {
      const r = cmdAgents({
        coordRoot: coord.root,
        ...(args.status !== undefined && { status: args.status }),
        ...(args.enrich !== undefined && { enrich: args.enrich }),
      });
      const count = r.items.length;
      const summary = count === 1 ? '1 agent' : `${count} agents`;
      return buildToolResult({
        summary,
        value: { members: r.items },
      });
    });

  // New canonical name. Dual-prefixed: coord_agents + st_agents.
  registerDualTool(
    mcp,
    'agents',
    {
      title: 'Enumerate coord agents',
      description:
        "Equivalent to `coord agents`. Enumerate agents present in $ST_ROOT with their effective status. Pass `enrich: true` to include `lastActivity` and inbox unread count per agent. Useful for peer discovery before sending — call this when you need to know who's available to message.",
      inputSchema: agentsInputShape,
      outputSchema: agentsOutputShape,
    },
    handler
  );

  // Deprecated alias for back-compat. Dual-prefixed: coord_members + st_members.
  registerDualTool(
    mcp,
    'members',
    {
      title: 'Enumerate coord agents (deprecated alias of `agents`)',
      description:
        "Deprecated alias of `coord_agents`. Behaves identically. Migrate boot rituals from `coord_members` / `st_members` to `coord_agents` / `st_agents` when convenient.",
      inputSchema: agentsInputShape,
      outputSchema: agentsOutputShape,
    },
    handler
  );
}

/** @deprecated Use {@link registerAgentsTool}. */
export const registerMembersTool = registerAgentsTool;
