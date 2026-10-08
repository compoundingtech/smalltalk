import { applyWindow, isTransientCode, plainError, plainMessage, type Agent, type Attention, type CollectionFrame, type Glass, type CollectionName, type CollectionStream, type CollectionWindow, type Mission, type Snapshot, type St3Client, type TerminalScreen, type TimelineEntry } from '../../clients/typescript/st3-client';
import { TERMINAL_RESTARTED, withFreshTerminalFence, type Foreground, type TerminalFollowHandlers } from './terminalControls';

// The app holds three windows on one collections socket. It needs no work window: missions carry
// their steps and agents name theirs. Subscription IDs are the window names.
export const FEED_WINDOWS = {
  attention: { collection: 'attention', limit: 200 },
  missions: { collection: 'missions', limit: 200 },
  agents: { collection: 'agents', limit: 200 },
} as const satisfies Record<string, { collection: CollectionName; limit: number }>;
export type FeedWindow = keyof typeof FEED_WINDOWS;
export type FeedLists = { attention: Attention[]; missions: Mission[]; agents: Agent[] };
const KINDS = { attention: 'attention', missions: 'mission', agents: 'agent' } as const;
const TERMINAL = 'terminal', CONVERSATION = 'conversation', GLASSES = 'glasses';
export const RETRY_DELAYS_MS = [1_000, 2_000, 5_000, 10_000, 30_000];

export type FeedHandlers = {
  /** A window's whole ordered list, after each frame that changed it. */
  onWindow: <K extends FeedWindow>(name: K, items: FeedLists[K], hasMore: boolean, snapshot: Snapshot) => void;
  /** `live` once a socket delivers its first snapshot; `reconnecting` while a dropped one waits. */
  onConnection: (state: 'connecting' | 'live' | 'reconnecting', issue?: string) => void;
  /** The server refused a window; the others keep going. */
  onWindowError?: (name: FeedWindow, message: string) => void;
  /** Development measurement hook; never carries transcript content or a target. */
  onConversationFrame?: (rows: number, replace: boolean) => void;
};

/** The person's glasses, all of them, after each frame that changed them. */
export type GlassesHandlers = {
  onGlasses: (glasses: Glass[]) => void;
  /** A problem to show; an empty string clears it. */
  onIssue: (issue: string) => void;
};

export type ConversationFrame = { replace: boolean; items: TimelineEntry[]; hasMore: boolean; sessionId?: string };
export type ConversationHandlers = {
  /** `replace` carries the newest page; otherwise the entries changed since the last frame. */
  onEntries: (frame: ConversationFrame) => void;
  /** A problem to show; an empty string clears it. */
  onIssue: (issue: string) => void;
};

type Client = Pick<St3Client, 'collectionStream' | 'terminalScreen' | 'terminalAttach' | 'terminalDetach'> & Partial<Pick<St3Client, 'capabilities'>>;
/** How often a live socket is checked, and how long one check may take. */
const PROBE_EVERY_MS = 10_000, PROBE_WAIT_MS = 5_000, QUIET_BEFORE_PROBE_MS = 10_000;

/** Whether st is asked anything: not while frames arrive, only once the stream has been quiet. */
export function shouldProbe(lastFrameAt: number, now: number): boolean {
  return now - lastFrameAt >= QUIET_BEFORE_PROBE_MS;
}
type Follow = { close(): void };

function errorCode(error: unknown): string | undefined {
  if (!error || typeof error !== 'object' || !('response' in error)) return undefined;
  const response = (error as { response?: { code?: unknown } }).response;
  return typeof response?.code === 'string' ? response.code : undefined;
}
// What the app shows and retries follows the SDK, so no raw code reaches the person.
function errorMessage(error: unknown): string {
  const code = errorCode(error);
  return code ? plainMessage(code, error instanceof Error ? error.message : String(error)) : plainError(error);
}
const transient = (code: string | undefined) => code !== undefined && isTransientCode(code);
/** Whether a refused conversation may load if asked again: all but st's word that it keeps no
 * start for the transcript (`timeline-history-incomplete`). */
export const conversationMayClear = (code: string | undefined) => code !== 'timeline-history-incomplete';

// One collections socket per paired gateway and credential. It holds the app's windows, at most one
// followed terminal, and at most one followed conversation. A dropped socket reconnects after
// 1/2/5/10/30 s, reset by a snapshot, and subscribes everything again: the new snapshots replace
// the lists. The socket lives only in the foreground: leaving it closes the socket and schedules
// nothing, and returning opens a fresh one. Nothing is read or sent while no frame arrives.
export class Feed {
  private closed = false;
  private attempt = 0;
  private failures = 0;
  private live = false;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private stream: CollectionStream | undefined;
  private windows: Partial<Record<FeedWindow, CollectionWindow>> = {};
  private terminal: TerminalFollow | undefined;
  private conversation: { target: string; handlers: ConversationHandlers; failures: number; timer?: ReturnType<typeof setTimeout> } | undefined;
  private glasses: { handlers: GlassesHandlers; window?: CollectionWindow } | undefined;
  /** Windows (and the glasses) st stopped sending: how often they failed, and the timer to ask again. */
  private retries: Partial<Record<FeedWindow | typeof GLASSES, { failures: number; timer?: ReturnType<typeof setTimeout> }>> = {};
  // A reason st gave for a window it cannot serve yet, said only once it has lasted a moment: a write
  // briefly revokes a source's readiness, and that blip is not worth a message.
  private reports: Partial<Record<FeedWindow, ReturnType<typeof setTimeout>>> = {};
  private readonly reportAfterMs: number;
  private probe: ReturnType<typeof setInterval> | undefined;
  /** When a frame last arrived on the open socket. */
  private lastFrameAt = Date.now();
  private readonly unsubscribe: () => void;
  private readonly client: Client;
  private readonly handlers: FeedHandlers;
  private readonly foreground: Foreground;
  private readonly newActionId: () => string;
  private readonly retryDelaysMs: readonly number[];

  constructor(client: Client, handlers: FeedHandlers, foreground: Foreground, newActionId: () => string, retryDelaysMs: readonly number[] = RETRY_DELAYS_MS, reportAfterMs = 1000) {
    this.reportAfterMs = reportAfterMs;
    this.client = client;
    this.handlers = handlers;
    this.foreground = foreground;
    this.newActionId = newActionId;
    this.retryDelaysMs = retryDelaysMs;
    this.unsubscribe = foreground.subscribe(active => {
      if (this.closed) return;
      if (active) { this.failures = 0; void this.connect(); } else this.suspend();
    });
    if (foreground.active) void this.connect();
  }

  /** Open a fresh socket now, for a person who asks to reconnect. */
  reconnect(): void {
    if (this.closed || !this.foreground.active) return;
    this.failures = 0;
    void this.connect();
  }

  close(): void {
    if (this.closed) return;
    this.terminal?.close();
    this.closed = true;
    this.suspend();
    this.unsubscribe();
  }

  /** Follow one terminal; a second call replaces the first. */
  followTerminal(terminalId: string, handlers: TerminalFollowHandlers): Follow {
    this.terminal?.close();
    const follow = new TerminalFollow(this.client, terminalId, handlers, this.newActionId, this.retryDelaysMs, () => this.stream, () => { if (this.terminal === follow) this.terminal = undefined; });
    this.terminal = follow;
    if (this.stream) void follow.attach();
    return { close: () => follow.close() };
  }

  /** Follow the conversation of an agent or a session; a second call replaces the first. */
  followConversation(target: string, handlers: ConversationHandlers): Follow {
    clearTimeout(this.conversation?.timer);
    const follow: NonNullable<Feed['conversation']> = { target, handlers, failures: 0 };
    this.conversation = follow;
    this.stream?.subscribeConversation(CONVERSATION, target);
    return { close: () => { if (this.conversation !== follow) return; clearTimeout(follow.timer); this.conversation = undefined; this.stream?.unsubscribe(CONVERSATION); } };
  }

  /** Follow the person's glasses (st keeps at most 100 live); a second call replaces the first. */
  followGlasses(handlers: GlassesHandlers): Follow {
    const follow = { handlers };
    this.glasses = follow;
    this.stream?.subscribeGlasses(GLASSES);
    return { close: () => { if (this.glasses !== follow) return; this.glasses = undefined; this.stream?.unsubscribe(GLASSES); } };
  }

  private suspend(): void {
    this.attempt++;
    clearTimeout(this.timer);
    this.timer = undefined;
    const open = this.stream;
    this.stream = undefined;
    this.live = false;
    this.windows = {};
    this.stopProbing();
    for (const retry of Object.values(this.retries)) clearTimeout(retry?.timer);
    this.retries = {};
    for (const report of Object.values(this.reports)) clearTimeout(report);
    this.reports = {};
    this.terminal?.socketLost();
    open?.close();
  }

  /**
   * A socket can stay open and silent while st is wedged or the network blackholed it, and the
   * app would say live with old data. Nothing is asked of st while the stream speaks: any frame
   * is proof of life. Only once it has been quiet for 10 s is st asked something small; a slow
   * answer is asked again at once, and only two misses in a row drop the socket.
   */
  private startProbing(): void {
    const capabilities = this.client.capabilities?.bind(this.client);
    if (this.probe || !capabilities) return;
    const current = this.attempt;
    const once = () => Promise.race([capabilities().then(() => true, () => false), new Promise<boolean>(resolve => setTimeout(() => resolve(false), PROBE_WAIT_MS))]);
    this.probe = setInterval(() => {
      if (!shouldProbe(this.lastFrameAt, Date.now())) return;
      void (async () => {
        if (await once() || await once()) return;
        if (current === this.attempt && !this.closed) this.dropped(new Error('st stopped answering'));
      })();
    }, PROBE_EVERY_MS);
  }

  private stopProbing(): void {
    clearInterval(this.probe);
    this.probe = undefined;
  }

  /** Ask again for a window st stopped sending, after a wait that grows; say so the first time only. */
  private retryLater(name: FeedWindow | typeof GLASSES, again: () => void): boolean {
    const retry = this.retries[name] ?? { failures: 0 };
    clearTimeout(retry.timer);
    const wait = this.retryDelaysMs[Math.min(retry.failures, this.retryDelaysMs.length - 1)];
    // Up to half again, so many clients told to ask again at once do not all ask together.
    const delay = wait + Math.floor(Math.random() * wait / 2);
    retry.failures++;
    retry.timer = setTimeout(() => { if (this.retries[name] === retry) { retry.timer = undefined; again(); } }, delay);
    this.retries[name] = retry;
    return retry.failures === 1;
  }

  private loaded(name: FeedWindow | typeof GLASSES): void {
    clearTimeout(this.retries[name]?.timer);
    delete this.retries[name];
    if (name !== GLASSES) { clearTimeout(this.reports[name]); delete this.reports[name]; }
  }

  private async connect(): Promise<void> {
    this.suspend();
    const current = this.attempt;
    const stale = () => this.closed || current !== this.attempt;
    this.handlers.onConnection(this.failures ? 'reconnecting' : 'connecting');
    try {
      const opened = await this.client.collectionStream({
        onFrame: frame => { if (!stale()) { this.lastFrameAt = Date.now(); this.frame(frame); } },
        onEnd: error => { if (!stale()) this.dropped(error ?? new Error('The collections socket closed.')); },
      });
      if (stale()) { opened.close(); return; }
      this.lastFrameAt = Date.now();
      this.stream = opened;
      for (const name of Object.keys(FEED_WINDOWS) as FeedWindow[]) this.subscribeWindow(name);
      if (this.conversation) opened.subscribeConversation(CONVERSATION, this.conversation.target);
      if (this.glasses) { this.glasses.window = undefined; opened.subscribeGlasses(GLASSES); }
      if (this.terminal) void this.terminal.attach();
    } catch (error) { if (!stale()) this.dropped(error); }
  }

  private subscribeWindow(name: FeedWindow): void {
    delete this.windows[name];
    this.stream?.subscribe(name, FEED_WINDOWS[name].collection, FEED_WINDOWS[name].limit);
  }

  private dropped(error: unknown): void {
    this.stream = undefined;
    this.live = false;
    this.windows = {};
    this.stopProbing();
    this.terminal?.socketLost();
    if (!this.foreground.active) return;
    const delay = this.retryDelaysMs[Math.min(this.failures, this.retryDelaysMs.length - 1)];
    this.failures++;
    this.handlers.onConnection('reconnecting', errorMessage(error));
    this.timer = setTimeout(() => { void this.connect(); }, delay);
  }

  private frame(frame: CollectionFrame): void {
    const id = 'id' in frame ? frame.id : undefined;
    if ((frame.kind === 'snapshot' || frame.kind === 'changes') && frame.id === GLASSES) {
      const follow = this.glasses;
      if (!follow) return;
      const next = applyWindow(follow.window, frame);
      if (!next) { follow.window = undefined; this.stream?.subscribeGlasses(GLASSES); return; }
      follow.window = next;
      this.loaded(GLASSES);
      follow.handlers.onIssue('');
      follow.handlers.onGlasses(next.items.filter(item => item.kind === 'glass') as Glass[]);
    } else if (frame.kind === 'snapshot' || frame.kind === 'changes') {
      if (!(frame.id in FEED_WINDOWS)) return;
      const name = frame.id as FeedWindow;
      const next = applyWindow(this.windows[name], frame);
      if (!next) { this.retryLater(name, () => this.subscribeWindow(name)); return; }
      this.windows[name] = next;
      this.loaded(name);
      if (frame.kind === 'snapshot') {
        this.failures = 0;
        if (!this.live) { this.live = true; this.handlers.onConnection('live'); this.startProbing(); }
      }
      const items = next.items.filter(item => item.kind === KINDS[name]) as FeedLists[typeof name];
      this.handlers.onWindow(name, items, next.hasMore, next.snapshot);
    } else if (frame.kind === 'resync') {
      // st keeps a conversation's subscription and retries it itself; it says why, so the last
      // copy shown can say it is stale (the owner's host is away).
      if (id === CONVERSATION && this.conversation) { if (frame.message) this.conversation.handlers.onIssue(`${plainMessage(frame.code, frame.message)} · trying again`); }
      else if (frame.id === GLASSES && this.glasses) { this.glasses.window = undefined; this.stream?.subscribeGlasses(GLASSES); }
      else if (frame.id in FEED_WINDOWS) {
        const name = frame.id as FeedWindow;
        if (this.retryLater(name, () => this.subscribeWindow(name)) && frame.message) {
          const text = `${plainMessage(frame.code, frame.message)} · trying again`;
          clearTimeout(this.reports[name]);
          this.reports[name] = setTimeout(() => { delete this.reports[name]; this.handlers.onWindowError?.(name, text); }, this.reportAfterMs);
        }
      }
    } else if (frame.kind === 'screen') {
      if (id === TERMINAL) this.terminal?.screen(frame.value);
    } else if (frame.kind === 'conversation') {
      if (id === CONVERSATION && this.conversation) {
        this.conversation.failures = 0;
        this.conversation.handlers.onIssue('');
        this.handlers.onConversationFrame?.(frame.items.length, frame.replace);
        this.conversation.handlers.onEntries({ replace: frame.replace, items: frame.items, hasMore: !!frame.has_more, sessionId: frame.session_id });
      }
    } else if (frame.kind === 'error') {
      if (id === TERMINAL) this.terminal?.failed(frame.code, frame.message);
      else if (id === CONVERSATION && this.conversation) {
        const follow = this.conversation;
        // st keeps no start for this transcript: asking again cannot help, so say st's reason
        // once and stop, as stui does.
        if (!conversationMayClear(frame.code)) {
          clearTimeout(follow.timer);
          follow.handlers.onIssue(plainMessage(frame.code, frame.message));
          return;
        }
        // The subscription ended. Whatever stopped it may clear (a busy store moved under the
        // page, the agent's host came back): ask again after a backoff, as stui does.
        const delay = this.retryDelaysMs[Math.min(follow.failures, this.retryDelaysMs.length - 1)];
        follow.failures++;
        clearTimeout(follow.timer);
        follow.timer = setTimeout(() => { if (this.conversation === follow) this.stream?.subscribeConversation(CONVERSATION, follow.target); }, delay);
        // A page that expired under a busy store is routine; say so only if it keeps happening.
        if (frame.code !== 'page-cursor-expired' || follow.failures > 2) follow.handlers.onIssue(`${plainMessage(frame.code, frame.message)} · trying again`);
      }
      else if (id === GLASSES && this.glasses) {
        // st stopped sending the glasses; for what may clear, ask again later rather than leave them stale.
        const plain = plainMessage(frame.code, frame.message);
        if (!isTransientCode(frame.code)) this.glasses.handlers.onIssue(plain);
        else if (this.retryLater(GLASSES, () => { if (this.glasses) { this.glasses.window = undefined; this.stream?.subscribeGlasses(GLASSES); } })) this.glasses.handlers.onIssue(`${plain} · trying again`);
      }
      else if (id && id in FEED_WINDOWS) {
        const name = id as FeedWindow;
        // A list whose first read failed is asked for again when that may help, so it never stays stale under "live".
        const plain = plainMessage(frame.code, frame.message);
        if (!isTransientCode(frame.code)) this.handlers.onWindowError?.(name, plain);
        else if (this.retryLater(name, () => this.subscribeWindow(name))) this.handlers.onWindowError?.(name, `${plain} · trying again`);
      }
    }
  }
}

// A terminal rides the feed's socket: attach, then subscribe with the capability the attachment
// returned. A `stale-fence` on the stream shows the restart notice and never reattaches (one on
// the attach itself only lost a race with a busy store, so it tries again); a transient error
// reattaches on the same socket; anything else ends following. After a dropped socket the feed
// attaches again, refusing a runtime now on another incarnation. Closing unsubscribes and detaches.
class TerminalFollow {
  private closed = false;
  private incarnation = '';
  private attempt = 0;
  private failures = 0;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private readonly client: Client;
  private readonly terminalId: string;
  private readonly handlers: TerminalFollowHandlers;
  private readonly newActionId: () => string;
  private readonly retryDelaysMs: readonly number[];
  /** The feed's current socket, if it has one. */
  private readonly stream: () => CollectionStream | undefined;
  private readonly released: () => void;

  constructor(client: Client, terminalId: string, handlers: TerminalFollowHandlers, newActionId: () => string, retryDelaysMs: readonly number[], stream: () => CollectionStream | undefined, released: () => void) {
    this.client = client;
    this.terminalId = terminalId;
    this.handlers = handlers;
    this.newActionId = newActionId;
    this.retryDelaysMs = retryDelaysMs;
    this.stream = stream;
    this.released = released;
  }

  async attach(): Promise<void> {
    this.cancel();
    const current = this.attempt;
    const stale = () => this.closed || current !== this.attempt;
    try {
      if (!this.incarnation) {
        // The first open learns the incarnation to fence to, and shows the screen at once.
        const screen = await this.client.terminalScreen(this.terminalId);
        if (stale()) return;
        this.incarnation = screen.value.runtime_incarnation;
        this.handlers.onScreen(screen.value);
      }
      const result = await withFreshTerminalFence(this.client, this.terminalId, this.incarnation, fence => {
        const id = this.newActionId();
        return this.client.terminalAttach({ id, idempotency_key: id, fence, parameters: { target_id: this.terminalId } });
      });
      if (stale()) return;
      const attachment = result.value.terminal_attachment;
      if (!attachment?.stream_capability) { this.stop('The gateway returned no terminal stream.'); return; }
      // A socket that dropped meanwhile attaches again when it reconnects.
      this.stream()?.subscribeTerminal(TERMINAL, this.terminalId, attachment.runtime_incarnation ?? this.incarnation, attachment.stream_capability);
    } catch (error) {
      if (stale()) return;
      const code = errorCode(error);
      if (error instanceof Error && error.message === TERMINAL_RESTARTED) this.stop(TERMINAL_RESTARTED);
      // The attach lost the race with a busy store (its fence is the store's whole index): try again.
      else if (code === 'stale-fence') this.retry('Attaching to a busy terminal; trying again.');
      else if (code && !transient(code)) this.stop(errorMessage(error));
      else this.retry(errorMessage(error));
    }
  }

  screen(screen: TerminalScreen): void {
    if (this.closed) return;
    if (screen.runtime_incarnation !== this.incarnation) { this.stop(TERMINAL_RESTARTED); return; }
    this.failures = 0;
    this.handlers.onIssue('');
    this.handlers.onScreen(screen);
  }

  failed(code: string | undefined, message: string): void {
    if (this.closed) return;
    // st also says stale-fence for an owner briefly out of reach or a viewer that idled, not only
    // a restart: attach again, and the attach itself refuses a terminal that really restarted.
    if (code === 'stale-fence' || transient(code)) this.retry(plainMessage(code, message));
    else this.stop(plainMessage(code, message));
  }

  /** The socket closed: whatever was in flight belongs to it. */
  socketLost(): void { this.cancel(); }

  close(): void {
    if (this.closed) return;
    this.stop('');
    this.stream()?.unsubscribe(TERMINAL);
    if (!this.incarnation) return;
    // The viewer record ends too; a terminal that restarted has no viewer to end.
    void withFreshTerminalFence(this.client, this.terminalId, this.incarnation, fence => {
      const id = this.newActionId();
      return this.client.terminalDetach({ id, idempotency_key: id, fence, parameters: { target_id: this.terminalId } });
    }).catch(() => {});
  }

  private retry(issue: string): void {
    const delay = this.retryDelaysMs[Math.min(this.failures, this.retryDelaysMs.length - 1)];
    this.failures++;
    this.handlers.onIssue(`Terminal stream interrupted; reconnecting (${issue}).`);
    this.timer = setTimeout(() => { void this.attach(); }, delay);
  }

  private cancel(): void {
    this.attempt++;
    clearTimeout(this.timer);
    this.timer = undefined;
  }

  private stop(issue: string): void {
    const wasClosed = this.closed;
    this.closed = true;
    this.cancel();
    this.released();
    if (!wasClosed && issue) {
      this.stream()?.unsubscribe(TERMINAL);
      this.handlers.onIssue(issue);
    }
  }
}
