// mcp/tools/reply.ts — registers the `coord_msg_reply` MCP tool.
//
// Channel-mode-only (Phase 2). Wraps a "find <thread> anywhere on the
// local tree, then coord.send back to its sender" pattern. The intent
// is: a Claude Code agent gets pinged via notifications/claude/channel,
// reads the meta.messageFilename, and calls coord_msg_reply({ thread,
// body }) to write a reply without having to think about identities.

import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { z } from 'zod';

import { EmptyBodyError } from '../../errors.ts';
import type { Coord } from '../../lib.ts';
import { locateThread } from '../../locate-thread.ts';
import { asFilename, asIdentity } from '../../types.ts';
import {
  buildToolResult,
  withErrorMapping,
} from '../error-mapping.ts';
import { registerDualTool } from './dual-register.ts';

const replyInputShape = {
  thread: z
    .string()
    .describe(
      'Filename of the message you are replying to (LAYOUT-004 grammar). The recipient is derived from that message\'s `from:` field.'
    ),
  body: z.string().describe('Reply body. Must be non-empty.'),
  subject: z
    .string()
    .optional()
    .describe(
      'Optional subject. If omitted, derived as `re: <original-subject>` from the threaded message (or omitted entirely if the original had no subject).'
    ),
};

const replyOutputShape = {
  filename: z
    .string()
    .describe('The new <unix-ms>-<rand6>.md filename written into the recipient\'s inbox.'),
  identity: z.string().describe('Recipient identity (the original `from`).'),
};

export function registerReplyTool(mcp: McpServer, coord: Coord): void {
  registerDualTool(
    mcp,
    'msg_reply',
    {
      title: 'Reply to a coord message',
      description:
        "Channel-mode-only. Write a reply to <thread>'s sender. Equivalent to `coord_msg_send` with `to` derived from the original's `from:` field, `inReplyTo: <thread>`, and a default `subject` of `re: <original-subject>`.",
      inputSchema: replyInputShape,
      outputSchema: replyOutputShape,
    },
    async (args) =>
      withErrorMapping(async () => {
        const thread = asFilename(args.thread);
        if (args.body.length === 0) {
          throw new EmptyBodyError();
        }
        const located = locateThread(coord.root, coord.identity, thread);
        const subject =
          args.subject !== undefined
            ? args.subject
            : located.subject !== undefined
              ? `re: ${located.subject}`
              : undefined;
        const sendOpts: Parameters<Coord['send']>[2] = {
          inReplyTo: thread,
        };
        if (subject !== undefined) sendOpts.subject = subject;
        const recipient = asIdentity(located.from);
        const filename = await coord.send(recipient, args.body, sendOpts);
        return buildToolResult({
          summary: `replied: ${recipient}/${filename}`,
          value: { filename, identity: recipient },
        });
      })
  );
}
