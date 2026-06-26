// mcp/tools/read.ts — registers the `coord_msg_read` MCP tool.

import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { z } from 'zod';

import type { Coord } from '../../lib.ts';
import { asFilename, asIdentity } from '../../types.ts';
import {
  buildToolResult,
  withErrorMapping,
} from '../error-mapping.ts';
import { registerDualTool } from './dual-register.ts';

const readInputShape = {
  filename: z
    .string()
    .describe('Message filename (LAYOUT-004 grammar). Required.'),
  identity: z
    .string()
    .optional()
    .describe(
      'Whose folder to read from. Defaults to $COORD_IDENTITY.'
    ),
  fromArchive: z
    .boolean()
    .optional()
    .describe(
      'When true, prefer archive/ first (auto-falls back to inbox).'
    ),
};

const readOutputShape = {
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
};

export function registerReadTool(mcp: McpServer, coord: Coord): void {
  registerDualTool(
    mcp,
    'msg_read',
    {
      title: 'Read one coord message',
      description:
        "Equivalent to `coord message read`. Return the parsed message at <identity>/inbox/<filename> (or archive, with auto-fallback). identity defaults to $COORD_IDENTITY.",
      inputSchema: readInputShape,
      outputSchema: readOutputShape,
    },
    async (args) =>
      withErrorMapping(async () => {
        const identity =
          args.identity !== undefined
            ? asIdentity(args.identity)
            : coord.identity;
        const filename = asFilename(args.filename);
        const opts: Parameters<Coord['read']>[2] = {};
        if (args.fromArchive !== undefined) opts.fromArchive = args.fromArchive;
        const r = await coord.read(identity, filename, opts);
        // r.message has optional fields that are sometimes absent — strip
        // undefined keys so the structuredContent reflects the actual
        // shape on disk (zod outputSchema accepts the optionals).
        const message: Record<string, unknown> = {
          from: r.message.from,
          body: r.message.body,
        };
        if (r.message.subject !== undefined) message.subject = r.message.subject;
        if (r.message.inReplyTo !== undefined) {
          message.inReplyTo = r.message.inReplyTo;
        }
        if (r.message.tags !== undefined) message.tags = r.message.tags;
        if (r.message.priority !== undefined) {
          message.priority = r.message.priority;
        }
        return buildToolResult({
          summary: `${r.folder}/${r.identity}/${r.filename}`,
          value: {
            filename: r.filename,
            identity: r.identity,
            folder: r.folder,
            message,
          },
        });
      })
  );
}
