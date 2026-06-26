// mcp/tools/thread.ts — registers the `coord_msg_thread` MCP tool.
//
// Wraps coord.thread(identity, filename, opts?) which walks the
// in-reply-to graph (both directions, cross-identity) and returns an
// array of MessageWithLocation. Default = flat chronological;
// tree=true keeps the bash --tree shape (depth in each line).

import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { z } from 'zod';

import type { Coord } from '../../lib.ts';
import { asFilename, asIdentity } from '../../types.ts';
import {
  buildToolResult,
  withErrorMapping,
} from '../error-mapping.ts';
import { registerDualTool } from './dual-register.ts';

const threadInputShape = {
  filename: z
    .string()
    .describe('Seed filename (LAYOUT-004 grammar). Required.'),
  identity: z
    .string()
    .optional()
    .describe(
      "Identity slot (mostly a validation hint — the walk is global). Defaults to $COORD_IDENTITY."
    ),
  tree: z
    .boolean()
    .optional()
    .describe(
      'When true, return depth-indented hierarchical view (vs flat chronological default).'
    ),
};

const threadOutputShape = {
  messages: z.array(
    z.object({
      filename: z.string(),
      identity: z.string(),
      folder: z.enum(['inbox', 'archive']),
      message: z.object({
        from: z.string(),
        subject: z.string().optional(),
        inReplyTo: z.string().optional(),
        tags: z.array(z.string()).optional(),
        priority: z.enum(['low', 'normal', 'high']).optional(),
        body: z.string(),
      }),
    })
  ),
};

export function registerThreadTool(mcp: McpServer, coord: Coord): void {
  registerDualTool(
    mcp,
    'msg_thread',
    {
      title: 'Walk a coord thread',
      description:
        "Equivalent to `coord message thread`. Return every message reachable from <filename> via in-reply-to (both directions, cross-identity). Default = flat chronological; tree=true preserves the depth-indented hierarchy.",
      inputSchema: threadInputShape,
      outputSchema: threadOutputShape,
    },
    async (args) =>
      withErrorMapping(async () => {
        const filename = asFilename(args.filename);
        const identity =
          args.identity !== undefined
            ? asIdentity(args.identity)
            : coord.identity;
        const opts: Parameters<Coord['thread']>[2] = {};
        if (args.tree !== undefined) opts.tree = args.tree;
        const messages = await coord.thread(identity, filename, opts);
        // Strip undefined-valued optional fields from each message so the
        // structuredContent matches what's on disk.
        const messagesOut = messages.map((m) => {
          const message: Record<string, unknown> = {
            from: m.message.from,
            body: m.message.body,
          };
          if (m.message.subject !== undefined) {
            message.subject = m.message.subject;
          }
          if (m.message.inReplyTo !== undefined) {
            message.inReplyTo = m.message.inReplyTo;
          }
          if (m.message.tags !== undefined) message.tags = m.message.tags;
          if (m.message.priority !== undefined) {
            message.priority = m.message.priority;
          }
          return {
            filename: m.filename,
            identity: m.identity,
            folder: m.folder,
            message,
          };
        });
        const summary =
          messages.length === 1
            ? '1 message in thread'
            : `${messages.length} messages in thread`;
        return buildToolResult({
          summary,
          value: { messages: messagesOut },
        });
      })
  );
}
