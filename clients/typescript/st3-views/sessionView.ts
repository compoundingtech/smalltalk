import type { Page, Session } from '@smalltalk/st3-client';

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

export function recentTimeline<T extends Pick<Entry, 'id' | 'sequence' | 'timestamp'>>(entries: T[], limit = 100): T[] {
  return [...entries].sort(before).slice(-limit);
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

type Entry = { id: string; sequence: number; type: string; body: unknown; timestamp?: string };

/** Reading back past the live window, one page of st's session timeline at a time. */
export type Older = {
  /** At least one earlier page was read: these entries are not in the live window. */
  paged: boolean;
  /** st said there is nothing before the oldest entry held: the session's start. */
  start: boolean;
  /** st's cursor for the page before the oldest one read, and when (ms) it was read. */
  cursor?: { value: string; at: number };
  loading: boolean;
  /** Why the last page could not be read; scrolling up tries again. */
  failed?: string;
};
export type Conversation<T extends Entry> = { entries: T[]; hasOlder: boolean; newestSequence: number; sessionId?: string; older: Older };
const noOlder: Older = { paged: false, start: false, loading: false };

// Everything a person reads in a conversation: the harness's turns, tool calls and results, Small
// Talk st joins in, and st's diagnostics. Status heartbeats and usage are not conversation.
export function isConversational(entry: Entry): boolean {
  return entry.type !== 'status' && entry.type !== 'usage';
}

/** st's presentation order; source sequences from different namespaces are only time ties. */
function before(a: Pick<Entry, 'id' | 'sequence' | 'timestamp'>, b: Pick<Entry, 'id' | 'sequence' | 'timestamp'>): number {
  const left = Date.parse(a.timestamp ?? '');
  const right = Date.parse(b.timestamp ?? '');
  const leftTime = Number.isFinite(left) ? left : Number.NEGATIVE_INFINITY;
  const rightTime = Number.isFinite(right) ? right : Number.NEGATIVE_INFINITY;
  if (leftTime !== rightTime) return leftTime < rightTime ? -1 : 1;
  return a.sequence - b.sequence || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0);
}

/** Whether st holds entries before the oldest one held. */
function moreBefore(older: Older, live: boolean): boolean {
  return older.paged ? !older.start : live;
}

// A followed conversation arrives as its newest page (`replace`), then as each change since. A
// revised entry replaces its earlier revision in place. Until the person reads back, only the
// newest `want` entries are kept and anything older is marked as older history; pages they read
// back are never dropped. A new newest page keeps those pages while it still meets them.
export function applyConversation<T extends Entry>(
  previous: Conversation<T> | undefined,
  frame: { replace: boolean; items: T[]; hasMore: boolean; sessionId?: string },
  want = 1000,
): Conversation<T> {
  const otherSession = !!frame.sessionId && !!previous?.sessionId && frame.sessionId !== previous.sessionId;
  const sessionId = frame.sessionId ?? previous?.sessionId;
  let older = previous && !otherSession ? previous.older : noOlder;
  let base: T[] = [];
  let live = previous?.hasOlder ?? frame.hasMore;
  if (!frame.replace && !otherSession) base = previous?.entries ?? [];
  else if (frame.replace) {
    live = frame.hasMore;
    const held = previous && !otherSession ? previous.entries : [];
    const meets = frame.items.some(item => held.some(entry => entry.id === item.id));
    const oldest = [...frame.items].sort(before)[0];
    if (older.paged && (meets || !frame.hasMore)) base = held.filter(entry => oldest && before(entry, oldest) < 0);
    else older = noOlder;
  }
  const found = new Map<string, T>(base.map(entry => [entry.id, entry]));
  let newestSequence = frame.replace || otherSession ? -1 : previous?.newestSequence ?? -1;
  for (const entry of frame.items) {
    newestSequence = Math.max(newestSequence, entry.sequence);
    if (isConversational(entry)) found.set(entry.id, entry);
    else found.delete(entry.id);
  }
  let entries = [...found.values()].sort(before);
  if (!older.paged && entries.length > want) {
    entries = entries.slice(-want);
    live = true;
  }
  return { entries, hasOlder: moreBefore(older, live), newestSequence, sessionId, older };
}

/** An earlier page of `sessionId`'s timeline; entries already held win. A page for another
 * session (the agent restarted meanwhile) is dropped. */
export function applyOlderPage<T extends Entry>(
  conversation: Conversation<T>,
  sessionId: string,
  page: { items: T[]; hasMore: boolean; cursor?: string },
  now = Date.now(),
): Conversation<T> {
  if (conversation.sessionId !== sessionId) return { ...conversation, older: { ...conversation.older, loading: false } };
  const found = new Map<string, T>(conversation.entries.map(entry => [entry.id, entry]));
  for (const entry of page.items) if (isConversational(entry) && !found.has(entry.id)) found.set(entry.id, entry);
  const older: Older = { paged: true, start: !page.hasMore, loading: false, ...(page.cursor ? { cursor: { value: page.cursor, at: now } } : {}) };
  return { ...conversation, entries: [...found.values()].sort(before), hasOlder: moreBefore(older, true), older };
}

export function olderLoading<T extends Entry>(conversation: Conversation<T>): Conversation<T> {
  return { ...conversation, older: { ...conversation.older, loading: true, failed: undefined } };
}

export function olderFailed<T extends Entry>(conversation: Conversation<T>, reason: string): Conversation<T> {
  return { ...conversation, older: { ...conversation.older, loading: false, failed: reason } };
}

/** st keeps a page cursor for five minutes; one older than this starts again from the newest. */
const CURSOR_LIFE_MS = 240_000;
/** Entries per page read back: st's largest, so a long session takes few requests. */
export const OLDER_PAGE = 200;

/**
 * The page before `oldest`. A live cursor continues where the last page ended. Without one (or
 * once st has let it go), pages are read again from the newest until one reaches past `oldest`,
 * so nothing between is skipped.
 */
export async function readOlder<T extends Entry>(
  read: (cursor?: string) => Promise<{ items: T[]; hasMore: boolean; cursor?: string }>,
  older: Older,
  oldest: T | undefined,
  now = Date.now(),
): Promise<{ items: T[]; hasMore: boolean; cursor?: string }> {
  if (older.cursor && now - older.cursor.at < CURSOR_LIFE_MS) {
    try { return await read(older.cursor.value); } catch (error) { if (!isSnapshotChurn(error)) throw error; }
  }
  let cursor: string | undefined;
  // Bounded: a session longer than this many pages stops loading with a reason.
  for (let hop = 0; hop < 50; hop++) {
    const page = await read(cursor);
    const first = [...page.items].sort(before)[0];
    if (!oldest || (first && before(first, oldest) < 0) || !page.hasMore || !page.cursor) return page;
    cursor = page.cursor;
  }
  throw new Error('this session is too long to read further back here; st conversations timeline reads it all');
}

/** The quiet line above the oldest entry: how to see more, that more is on its way, why it could
 * not come, or that this is where the session starts. */
export function olderNote(conversation: Conversation<Entry>): string | null {
  const { older } = conversation;
  if (older.loading) return 'Loading earlier entries…';
  if (older.failed) return `Could not load earlier entries: ${older.failed} · scroll up to try again`;
  if (conversation.hasOlder) return 'Scroll up for earlier entries';
  if (!conversation.entries.length) return null;
  return 'Start of this session · earlier ones: st conversations sessions';
}

export type ConversationRow<T> = { kind: 'older' } | { kind: 'entry'; entry: T };
export function conversationRows<T>(entries: T[], hasOlder: boolean): ConversationRow<T>[] {
  return [...(hasOlder ? [{ kind: 'older' as const }] : []), ...entries.map(entry => ({ kind: 'entry' as const, entry }))];
}
