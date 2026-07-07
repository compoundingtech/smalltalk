// mcp/tools/agents.ts — registers the `st_agents` MCP tool.
//
// Thin wrapper over `cmdAgents` from commands/agents.ts. The CLI entry
// point (cmdAgentsCli) and this tool share the same pure enumeration;
// no shelling out.
//
// The output payload still uses the field name `members:` in the
// structured content — that's a wire shape kept for back-compat; will
// rename to `agents:` in a follow-up.

import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { z } from 'zod';

import { cmdAgents } from '../../commands/agents.ts';
import type { St } from '../../lib.ts';
import { STATES } from '../../types.ts';
import {
  buildToolResult,
  withErrorMapping,
} from '../error-mapping.ts';

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

export function registerAgentsTool(mcp: McpServer, coord: St): void {
  const handler = async (args: { status?: string; enrich?: boolean }) =>
    withErrorMapping(async () => {
      const r = cmdAgents({
        stRoot: coord.root,
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

  mcp.registerTool(
    'st_agents',
    {
      title: 'Enumerate smalltalk agents',
      description:
        "Equivalent to `st agents`. Enumerate agents present in $ST_ROOT with their effective status. Pass `enrich: true` to include `lastActivity` and inbox unread count per agent. Useful for peer discovery before sending — call this when you need to know who's available to message.",
      inputSchema: agentsInputShape,
      outputSchema: agentsOutputShape,
    },
    handler
  );
}
