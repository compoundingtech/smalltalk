// mcp/tools/send.ts — registers the `coord_msg_send` MCP tool.
//
// Mirrors the `coord.send(to, body, opts)` API method 1:1. Required:
// to, body. Optional: from, subject, inReplyTo, tags[], priority.
// Returns { filename, identity } in structuredContent on success.

import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { z } from 'zod';

import type { Coord } from '../../lib.ts';
import { asFilename, asIdentity, type Filename } from '../../types.ts';
import {
  buildToolResult,
  withErrorMapping,
} from '../error-mapping.ts';
import { registerDualTool } from './dual-register.ts';

const sendInputShape = {
  to: z.string().describe('Recipient identity (LAYOUT-004 grammar)'),
  body: z.string().describe('Message body (markdown). Required.'),
  from: z
    .string()
    .optional()
    .describe('Sender identity. Defaults to $COORD_IDENTITY.'),
  subject: z.string().optional().describe('Optional subject line.'),
  inReplyTo: z
    .string()
    .optional()
    .describe(
      'Filename of the message being replied to (LAYOUT filename grammar).'
    ),
  tags: z
    .array(z.string())
    .optional()
    .describe('Optional list of tag strings.'),
  priority: z
    .enum(['low', 'normal', 'high'])
    .optional()
    .describe('Optional priority hint.'),
};

const sendOutputShape = {
  filename: z
    .string()
    .describe('The generated <unix-ms>-<rand6>.md filename.'),
  identity: z
    .string()
    .describe('The recipient identity the file was written under.'),
};

export function registerSendTool(mcp: McpServer, coord: Coord): void {
  registerDualTool(
    mcp,
    'msg_send',
    {
      title: 'Send a coord message',
      description:
        'Equivalent to `coord message send`. Write a new message to <to>/inbox/. The act of writing IS the send; sync moves the file across machines later.',
      inputSchema: sendInputShape,
      outputSchema: sendOutputShape,
    },
    async (args) =>
      withErrorMapping(async () => {
        const to = asIdentity(args.to);
        const opts: Parameters<Coord['send']>[2] = {};
        if (args.from !== undefined) opts.from = asIdentity(args.from);
        if (args.subject !== undefined) opts.subject = args.subject;
        if (args.inReplyTo !== undefined) {
          opts.inReplyTo = asFilename(args.inReplyTo);
        }
        if (args.tags !== undefined) opts.tags = args.tags;
        if (args.priority !== undefined) opts.priority = args.priority;
        const filename: Filename = await coord.send(to, args.body, opts);
        return buildToolResult({
          summary: `sent: ${to}/${filename}`,
          value: { filename, identity: to },
        });
      })
  );
}
