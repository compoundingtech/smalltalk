// mcp/tools/members.ts — registers the `coord_members` MCP tool.
//
// Thin wrapper over `cmdMembers` from commands/members.ts. The CLI
// entry point (cmdMembersCli) and this tool share the same pure
// enumeration; no shelling out.

import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { z } from 'zod';

import { cmdMembers } from '../../commands/members.ts';
import type { Coord } from '../../lib.ts';
import { STATES } from '../../types.ts';
import {
  buildToolResult,
  withErrorMapping,
} from '../error-mapping.ts';
import { registerDualTool } from './dual-register.ts';

const membersInputShape = {
  status: z
    .enum(STATES)
    .optional()
    .describe(
      'Filter to identities whose effective status matches. ' +
        '`unknown` is the derived state when a peer\'s status-file ' +
        'mtime is older than ~15 minutes. Default: all.'
    ),
  enrich: z
    .boolean()
    .optional()
    .describe(
      'Include lastActivity and inbox unread count per identity.'
    ),
};

const memberShape = {
  identity: z.string(),
  status: z.enum(STATES),
  name: z.string().nullable(),
  lastActivity: z
    .number()
    .nullable()
    .optional()
    .describe('Newest mtime under <id>/ across inbox/archive/status. Enriched only.'),
  inbox: z
    .number()
    .int()
    .nonnegative()
    .optional()
    .describe('Count of valid-grammar files in <id>/inbox/. Enriched only.'),
};

const membersOutputShape = {
  members: z
    .array(z.object(memberShape))
    .describe('Identities under $COORD_ROOT, sorted alphabetically.'),
};

export function registerMembersTool(mcp: McpServer, coord: Coord): void {
  registerDualTool(
    mcp,
    'members',
    {
      title: 'Enumerate coord identities',
      description:
        'Equivalent to `coord members`. Enumerate identities present in $COORD_ROOT with their effective status. Pass `enrich: true` to include `lastActivity` and inbox unread count per identity. Useful for peer discovery before sending — call this when you need to know who\'s available to message.',
      inputSchema: membersInputShape,
      outputSchema: membersOutputShape,
    },
    async (args) =>
      withErrorMapping(async () => {
        const r = cmdMembers({
          coordRoot: coord.root,
          ...(args.status !== undefined && { status: args.status }),
          ...(args.enrich !== undefined && { enrich: args.enrich }),
        });
        const count = r.items.length;
        const summary =
          count === 1 ? '1 identity' : `${count} identities`;
        return buildToolResult({
          summary,
          value: { members: r.items },
        });
      })
  );
}
