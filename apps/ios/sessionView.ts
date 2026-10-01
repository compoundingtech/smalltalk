import type { Page, Session } from '../../clients/typescript/st3-client';

// The gateway's discovery fields are additional properties on the generated
// st3.client.v0 Session resource. Keep this interpretation at the UI boundary.
export type SessionView = Session & {
  managed?: false;
  driver?: string;
  native_session_id?: string | null;
  title?: string | null;
  process?: { pid: number; exact_session: boolean } | null;
};

export function isUnmanaged(session: SessionView): boolean {
  return session.managed === false;
}

export function isUnresolved(session: SessionView): boolean {
  return isUnmanaged(session) && session.native_session_id == null;
}

export function isSnapshotChurn(error: unknown): boolean {
  return typeof error === 'object' && error !== null && 'response' in error
    && typeof error.response === 'object' && error.response !== null
    && 'code' in error.response && error.response.code === 'page-cursor-expired';
}

export function recentTimeline<T extends { sequence: number }>(entries: T[], limit = 100): T[] {
  return [...entries].sort((a, b) => a.sequence - b.sequence).slice(-limit);
}

export function timelineText(body: unknown): string | null {
  if (typeof body === 'string') return body;
  if (typeof body === 'object' && body !== null && 'text' in body && typeof body.text === 'string') return body.text;
  return null;
}

export function sessionLabel(session: SessionView, sourceHost: string): string {
  if (!isUnmanaged(session)) return `Declared · ${session.owner_id}`;
  const driver = session.driver ?? 'Native harness';
  if (isUnresolved(session)) return `Undeclared · unresolved ${driver} process · ${sourceHost}`;
  return `Undeclared · ${driver} session · ${sourceHost}`;
}

export function sessionDetail(session: SessionView): string {
  if (isUnresolved(session)) return `Running process${session.process ? ` PID ${session.process.pid}` : ''} · exact session unknown`;
  return `${session.state}${session.title ? ` · ${session.title}` : ''} · ${session.id}`;
}

export async function listSessionPages(
  list: (options: { limit: number; cursor?: string; history?: boolean }) => Promise<{ value: Page }>,
  limit: number,
  history = false,
): Promise<SessionView[]> {
  for (let attempt = 0; attempt < 3; attempt++) {
    const sessions: SessionView[] = [];
    const seen = new Set<string>();
    let cursor: string | undefined;
    try {
      for (let pageNumber = 0; pageNumber < 5; pageNumber++) {
        const page = (await list({ limit, cursor, history })).value;
        sessions.push(...page.items.filter((item): item is Session => item.kind === 'session'));
        if (!page.page.has_more) break;
        const next = page.page.next_cursor;
        if (!next || seen.has(next)) throw new Error('Session pagination did not advance.');
        seen.add(next);
        cursor = next;
      }
      return sessions;
    } catch (error) {
      if (!isSnapshotChurn(error) || attempt === 2) throw error;
      await new Promise(resolve => setTimeout(resolve, 100 * (attempt + 1)));
    }
  }
  throw new Error('Session pagination retries exhausted.');
}

type Entry = { id: string; sequence: number; type: string; body: unknown };
export type Conversation<T extends Entry> = { entries: T[]; hasOlder: boolean; newestSequence: number };

// Everything a person reads in a conversation: the harness's turns, tool calls and results, Small
// Talk st joins in, and st's diagnostics. Status heartbeats and usage are not conversation.
export function isConversational(entry: Entry): boolean {
  return entry.type !== 'status' && entry.type !== 'usage';
}

// A followed conversation arrives as its newest page (`replace`), then as each change since. A
// revised entry replaces its earlier revision in place. Only the newest `want` entries are kept,
// and anything older is marked as older history.
export function applyConversation<T extends Entry>(
  previous: Conversation<T> | undefined,
  frame: { replace: boolean; items: T[]; hasMore: boolean },
  want = 200,
): Conversation<T> {
  const base = frame.replace ? undefined : previous;
  const found = new Map<string, T>(base?.entries.map(entry => [entry.id, entry]));
  let newestSequence = base?.newestSequence ?? -1;
  for (const entry of frame.items) {
    newestSequence = Math.max(newestSequence, entry.sequence);
    if (isConversational(entry)) found.set(entry.id, entry);
    else found.delete(entry.id);
  }
  const entries = [...found.values()].sort((a, b) => a.sequence - b.sequence);
  const hasOlder = (base ? base.hasOlder : frame.hasMore) || entries.length > want;
  return { entries: entries.slice(-want), hasOlder, newestSequence };
}

export type ConversationRow<T> = { kind: 'older' } | { kind: 'entry'; entry: T };
export function conversationRows<T>(entries: T[], hasOlder: boolean): ConversationRow<T>[] {
  return [...(hasOlder ? [{ kind: 'older' as const }] : []), ...entries.map(entry => ({ kind: 'entry' as const, entry }))];
}
