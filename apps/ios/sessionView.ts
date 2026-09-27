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
type TimelinePage<T> = { value: { items: T[]; page: { has_more: boolean; next_cursor?: string | null } } };
export type Conversation<T extends Entry> = { entries: T[]; hasOlder: boolean; newestSequence: number };

export function isConversational(entry: Entry): boolean {
  return entry.type === 'content' && !!timelineText(entry.body)?.trim();
}

// Timeline pages arrive newest first. Status heartbeats, tool calls, and usage can fill whole pages,
// so keep reading older pages (bounded) until enough conversation is visible. A later poll stops at
// the newest entry it already has and merges, instead of re-reading deep history.
export async function loadRecentConversation<T extends Entry>(
  list: (options: { limit: number; cursor?: string }) => Promise<TimelinePage<T>>,
  { pageSize, maxPages, want, previous }: { pageSize: number; maxPages: number; want: number; previous?: Conversation<T> },
): Promise<Conversation<T>> {
  const known = previous && previous.newestSequence >= 0 ? previous : undefined;
  const found = new Map<string, T>();
  let cursor: string | undefined;
  let hasOlder = false;
  let newestSequence = known?.newestSequence ?? -1;
  let reachedKnown = false;
  for (let page = 0; page < maxPages; page++) {
    const result = (await list({ limit: pageSize, cursor })).value;
    for (const entry of result.items) {
      newestSequence = Math.max(newestSequence, entry.sequence);
      if (known && entry.sequence <= known.newestSequence) reachedKnown = true;
      // Known entries on this page are kept too: a streaming entry is revised in place.
      if (isConversational(entry)) found.set(entry.id, entry);
    }
    hasOlder = result.page.has_more && !!result.page.next_cursor;
    if (reachedKnown || found.size >= want || !hasOlder) break;
    cursor = result.page.next_cursor!;
  }
  if (known && reachedKnown) {
    for (const entry of known.entries) if (!found.has(entry.id)) found.set(entry.id, entry);
    hasOlder = known.hasOlder || found.size > want;
  } else if (found.size > want) hasOlder = true;
  const entries = [...found.values()].sort((a, b) => a.sequence - b.sequence).slice(-want);
  return { entries, hasOlder, newestSequence };
}

export type ConversationRow<T> = { kind: 'older' } | { kind: 'entry'; entry: T };
export function conversationRows<T>(entries: T[], hasOlder: boolean): ConversationRow<T>[] {
  return [...(hasOlder ? [{ kind: 'older' as const }] : []), ...entries.map(entry => ({ kind: 'entry' as const, entry }))];
}

type SessionMessage = { session_id?: string | null; from: string; to: string; sent_at: string };
export function sessionMessagesFor<T extends SessionMessage>(messages: T[], session: Pick<SessionView, 'id' | 'owner_id'>): T[] {
  return messages
    .filter(message => message.session_id === session.id || message.from === session.owner_id || message.to === session.owner_id)
    .sort((a, b) => a.sent_at.localeCompare(b.sent_at));
}
