// The simplified conversation, as st3-conversation-ui draws it (Density::Simple): a tool call is
// one line, and a run of calls in a row is one line saying how many and how they went until it
// is opened. Everything else reads as in full. The phone shows it by default; the choice is the
// device's own.
import type { ConversationEntry } from './conversationView';

type Tool = Extract<ConversationEntry['body'], { kind: 'tool' }>;
export type SimpleRow =
  | { kind: 'entry'; entry: ConversationEntry }
  | { kind: 'call'; entry: ConversationEntry; tool: Tool }
  | { kind: 'bundle'; id: string; calls: Array<{ entry: ConversationEntry; tool: Tool }>; ok: number; failed: number; running: number; last: string; open: boolean };

/** The id that opens a run of calls starting at `first`, as stui's `bundle_id`. */
export const bundleId = (first: string) => `bundle:${first}`;

/** Entries, oldest first, as simplified rows; `open` holds the ids of opened runs. */
export function simplify(entries: readonly ConversationEntry[], open: ReadonlySet<string>): SimpleRow[] {
  const rows: SimpleRow[] = [];
  for (let index = 0; index < entries.length;) {
    const calls: Array<{ entry: ConversationEntry; tool: Tool }> = [];
    while (index + calls.length < entries.length && entries[index + calls.length].body.kind === 'tool') {
      const entry = entries[index + calls.length];
      calls.push({ entry, tool: entry.body as Tool });
    }
    if (!calls.length) {
      rows.push({ kind: 'entry', entry: entries[index] });
      index += 1;
      continue;
    }
    index += calls.length;
    if (calls.length === 1) {
      rows.push({ kind: 'call', ...calls[0] });
      continue;
    }
    const id = bundleId(calls[0].entry.id);
    const count = (state: Tool['state']) => calls.filter(call => call.tool.state === state).length;
    rows.push({ kind: 'bundle', id, calls, ok: count('ok'), failed: count('failed'), running: count('running'), last: calls[calls.length - 1].tool.title, open: open.has(id) });
    if (open.has(id)) for (const call of calls) rows.push({ kind: 'call', ...call });
  }
  return rows;
}
