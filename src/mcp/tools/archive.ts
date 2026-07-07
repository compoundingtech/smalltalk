// mcp/tools/archive.ts — registers the `st_msg_archive` MCP tool.
//
// Mirrors `st.archive(identity, filename)`. Returns
// { outcome: 'moved' | 'idempotent' } so embedders can distinguish
// case-4 (clean rename) from case-2/0 (the file is already archived).

import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { z } from 'zod';

import { archiveDir, inboxDir } from '../../common.ts';
import type { St } from '../../lib.ts';
import { asDeliverableFilename, asIdentity } from '../../types.ts';
import {
  buildToolResult,
  withErrorMapping,
} from '../error-mapping.ts';

const archiveInputShape = {
  filename: z
    .string()
    .describe(
      "Message filename (LAYOUT-004 grammar, or an off-format `.md` delivered as an 'outside' message). Required."
    ),
  identity: z
    .string()
    .optional()
    .describe(
      "Whose folder to archive within. Defaults to $ST_AGENT."
    ),
};

const archiveOutputShape = {
  filename: z.string(),
  identity: z.string(),
  outcome: z
    .enum(['moved', 'idempotent'])
    .describe(
      "'moved' = case-4 clean rename, 'idempotent' = already archived (case 0 or case 2 byte-identical twin)."
    ),
};

export function registerArchiveTool(mcp: McpServer, st: St): void {
  mcp.registerTool(
    'st_msg_archive',
    {
      title: 'Archive a smalltalk message',
      description:
        "Equivalent to `st message archive`. Move <identity>/inbox/<filename> to <identity>/archive/<filename>. Idempotent on a byte-identical twin; refuses on divergent twin (ARCHIVE_CONFLICT).",
      inputSchema: archiveInputShape,
      outputSchema: archiveOutputShape,
    },
    async (args) =>
      withErrorMapping(async () => {
        const filename = asDeliverableFilename(args.filename);
        const identity =
          args.identity !== undefined
            ? asIdentity(args.identity)
            : st.identity;
        // The St.archive() return type is void; to surface "moved"
        // vs "idempotent" we re-check the inbox before calling. If the
        // file is already gone (post-sweep idempotent) AND the archive
        // copy is present, that's the idempotent outcome; otherwise a
        // successful call did the move.
        const ipath = join(inboxDir(identity, st.root), filename);
        const apath = join(archiveDir(identity, st.root), filename);
        const wasPresent = existsSync(ipath);
        const archivePresent = existsSync(apath);
        await st.archive(identity, filename);
        const outcome: 'moved' | 'idempotent' =
          wasPresent && !archivePresent ? 'moved' : 'idempotent';
        const summary =
          outcome === 'moved'
            ? `archived: ${identity}/${filename}`
            : `archived (idempotent): ${identity}/${filename}`;
        return buildToolResult({
          summary,
          value: { filename, identity, outcome },
        });
      })
  );
}
