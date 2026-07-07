// locate-thread.ts — resolve a message filename to its `from:` +
// `subject:` frontmatter so a reply can be threaded correctly.
//
// Used by BOTH `src/commands/reply.ts` (CLI `st message reply`) and
// `src/mcp/tools/reply.ts` (MCP `st_msg_reply` tool). Kept in one
// place so the two entry points can't drift on locate semantics.

import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';

import { parseFrontmatter, validIdentity } from './common.ts';
import { InvalidIdentityError, MessageNotFoundError } from './errors.ts';
import type { Identity } from './types.ts';

export interface LocatedMessage {
  from: Identity;
  subject?: string;
}

/**
 * Locate a message by filename across:
 * - `<root>/<selfIdentity>/inbox`
 * - `<root>/<selfIdentity>/archive`
 * - Every other identity tree's `archive/` (the cross-identity case
 *   after sync mirrors a peer's archived message back to your tree)
 *
 * Returns the parsed `from` + `subject` from the located file's
 * frontmatter. Throws `MessageNotFoundError` when the filename isn't
 * found in any of the searched locations; throws
 * `InvalidIdentityError` if the located file's `from:` field is
 * missing or malformed.
 */
export function locateThread(
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
