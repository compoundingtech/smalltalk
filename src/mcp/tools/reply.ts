// mcp/tools/reply.ts — registers the `coord_msg_reply` MCP tool.
//
// Channel-mode-only (Phase 2). Wraps a "find <thread> anywhere on the
// local tree, then coord.send back to its sender" pattern. The intent
// is: a Claude Code agent gets pinged via notifications/claude/channel,
// reads the meta.messageFilename, and calls coord_msg_reply({ thread,
// body }) to write a reply without having to think about identities.

import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { z } from 'zod';

import { parseFrontmatter, validIdentity } from '../../common.ts';
import {
  EmptyBodyError,
  InvalidIdentityError,
  MessageNotFoundError,
} from '../../errors.ts';
import type { Coord } from '../../lib.ts';
import { asFilename, asIdentity, type Identity } from '../../types.ts';
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

interface LocatedMessage {
  from: Identity;
  subject?: string;
}

/**
 * Locate a message by filename across:
 * - $COORD_IDENTITY/inbox
 * - $COORD_IDENTITY/archive
 * - every other identity tree's archive (the cross-identity case after
 *   sync mirrors a peer's archived message back to your tree)
 *
 * Returns the parsed `from` + `subject` so coord_msg_reply can build
 * the outbound message. Throws MessageNotFoundError if the file is
 * not found in any of those locations.
 */
function locateThread(
  root: string,
  selfIdentity: Identity,
  filename: string
): LocatedMessage {
  const ownInbox = join(root, selfIdentity, 'inbox', filename);
  const ownArchive = join(root, selfIdentity, 'archive', filename);
  const candidates: string[] = [ownInbox, ownArchive];

  let topEntries: string[];
  try {
    topEntries = readdirSync(root);
  } catch {
    topEntries = [];
  }
  for (const id of topEntries) {
    if (id === selfIdentity) continue;
    candidates.push(join(root, id, 'archive', filename));
  }

  for (const path of candidates) {
    if (!existsSync(path)) continue;
    const text = readFileSync(path, 'utf8');
    const { fm } = parseFrontmatter(text);
    const fromRaw = typeof fm.from === 'string' ? fm.from : '';
    if (fromRaw === '' || !validIdentity(fromRaw)) {
      throw new InvalidIdentityError(fromRaw);
    }
    const result: LocatedMessage = { from: fromRaw as Identity };
    if (typeof fm.subject === 'string' && fm.subject.length > 0) {
      result.subject = fm.subject;
    }
    return result;
  }
  throw new MessageNotFoundError(selfIdentity, filename);
}

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
