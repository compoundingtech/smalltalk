// mcp/tools/ls.ts — registers the `st_msg_ls` MCP tool.

import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { z } from 'zod';

import type { St } from '../../lib.ts';
import { asIdentity } from '../../types.ts';
import {
  buildToolResult,
  withErrorMapping,
} from '../error-mapping.ts';

const lsInputShape = {
  identity: z
    .string()
    .optional()
    .describe(
      'Whose inbox/archive to list. Defaults to $ST_AGENT.'
    ),
  archive: z
    .boolean()
    .optional()
    .describe('When true, list <identity>/archive/ instead of inbox/.'),
  since: z
    .number()
    .int()
    .nonnegative()
    .optional()
    .describe(
      'Filter to filenames whose <unix-ms> prefix is >= this value.'
    ),
  fromFilter: z
    .string()
    .optional()
    .describe(
      "Filter to messages whose `from:` frontmatter equals this identity."
    ),
};

const lsOutputShape = {
  matches: z.array(z.string()).describe('Matching filenames in chronological order.'),
  archive: z
    .boolean()
    .describe('Whether the listing was against archive/ (vs inbox/).'),
  identity: z.string().describe('Resolved identity that was listed.'),
};

export function registerLsTool(mcp: McpServer, st: St): void {
  mcp.registerTool(
    'st_msg_ls',
    {
      title: 'List inbox or archive',
      description:
        "Equivalent to `st message ls`. List filenames in <identity>/inbox/ (or <identity>/archive/ with archive=true). Filterable by --since (filename ts) and fromFilter (frontmatter `from:`).",
      inputSchema: lsInputShape,
      outputSchema: lsOutputShape,
    },
    async (args) =>
      withErrorMapping(async () => {
        const opts: Parameters<St['ls']>[1] = {};
        if (args.archive !== undefined) opts.archive = args.archive;
        if (args.since !== undefined) opts.since = args.since;
        if (args.fromFilter !== undefined) {
          opts.fromFilter = asIdentity(args.fromFilter);
        }
        const target =
          args.identity !== undefined ? asIdentity(args.identity) : st.identity;
        const matches = await st.ls(target, opts);
        const summary =
          matches.length === 1
            ? `1 message in ${args.archive === true ? 'archive' : 'inbox'}`
            : `${matches.length} messages in ${args.archive === true ? 'archive' : 'inbox'}`;
        return buildToolResult({
          summary,
          value: {
            matches,
            archive: args.archive === true,
            identity: target,
          },
        });
      })
  );
}
