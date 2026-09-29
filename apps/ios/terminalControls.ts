import type { St3Client, TerminalStream } from '../../clients/typescript/st3-client';
import type { Fence, TerminalScreen } from '../../clients/typescript/st3-client/Models.generated';

type TerminalFence = Fence & Required<Pick<Fence, 'runtime_incarnation' | 'terminal_sequence'>>;

function isStaleFence(error: unknown): boolean {
  return !!error && typeof error === 'object' && 'response' in error
    && !!error.response && typeof error.response === 'object'
    && 'code' in error.response && error.response.code === 'stale-fence';
}

// A terminal action changes the global store index. Never reuse a runtime-list
// fence or a prior action's screen sequence, and never redirect input into a
// new process that happens to reuse the same terminal ID.
export async function withFreshTerminalFence<T>(
  client: Pick<St3Client, 'terminalScreen'>,
  terminalId: string,
  expectedIncarnation: string,
  action: (fence: TerminalFence) => Promise<T>,
): Promise<T> {
  for (let attempt = 0; attempt < 3; attempt++) {
    const screen = await client.terminalScreen(terminalId);
    if (screen.value.runtime_incarnation !== expectedIncarnation) {
      throw new Error('Terminal restarted; reopen it before sending input.');
    }
    const fence: TerminalFence = {
      snapshot_id: screen.snapshot.id,
      subject_revisions: {},
      runtime_incarnation: expectedIncarnation,
      terminal_sequence: screen.value.next_sequence,
    };
    try { return await action(fence); }
    catch (error) { if (!isStaleFence(error) || attempt === 2) throw error; }
  }
  throw new Error('Terminal changed too often; try again.');
}

export const TERMINAL_RESTARTED = 'Terminal restarted; reopen it before sending input.';
const RETRY_DELAYS_MS = [1_000, 2_000, 5_000, 10_000, 30_000];

type Client = Pick<St3Client, 'terminalScreen' | 'terminalAttach' | 'terminalStream'>;

export type TerminalFollowHandlers = {
  /** Each screen replaces the one before it. */
  onScreen: (screen: TerminalScreen) => void;
  /** A problem to show; an empty string clears it. */
  onIssue: (issue: string) => void;
};

function errorCode(error: unknown): string | undefined {
  if (!error || typeof error !== 'object' || !('response' in error)) return undefined;
  const response = (error as { response?: { code?: unknown } }).response;
  return typeof response?.code === 'string' ? response.code : undefined;
}

function errorMessage(error: unknown): string {
  const code = errorCode(error);
  const message = error instanceof Error ? error.message : String(error);
  return code ? `${code}: ${message}` : message;
}

/** The app's foreground state, such as `ForegroundGate`. */
export type Foreground = {
  readonly active: boolean;
  subscribe(listener: (active: boolean) => void): () => void;
};

// Follow one terminal without polling: read its current screen, attach, then hold one stream
// open and show each screen the server sends. A dropped stream reattaches after a backoff and
// resumes from the current screen. Leaving the foreground closes the stream and schedules
// nothing; returning reads the current screen and attaches again. A new runtime incarnation
// (`stale-fence`) or a refused request stops following, because only reopening the terminal
// can resolve it.
export function followTerminal(
  client: Client,
  terminalId: string,
  handlers: TerminalFollowHandlers,
  newActionId: () => string,
  foreground: Foreground,
  retryDelaysMs: readonly number[] = RETRY_DELAYS_MS,
): { close(): void } {
  let closed = false, incarnation = '', failures = 0, attempt = 0;
  let stream: TerminalStream | undefined, timer: ReturnType<typeof setTimeout> | undefined;

  // Each open or suspension starts a new attempt, so work an older attempt still has in flight
  // never shows a screen or keeps a stream.
  function suspend() {
    attempt++;
    clearTimeout(timer);
    timer = undefined;
    const open = stream;
    stream = undefined;
    open?.close();
  }
  function stop(issue: string) { closed = true; suspend(); unsubscribe(); handlers.onIssue(issue); }
  function retry(error: unknown) {
    if (closed) return;
    const code = errorCode(error);
    if (code === 'stale-fence' || (error instanceof Error && error.message === TERMINAL_RESTARTED)) { stop(TERMINAL_RESTARTED); return; }
    if (code && code !== 'internal' && code !== 'remote-unavailable' && code !== 'rate-limited') { stop(errorMessage(error)); return; }
    if (!foreground.active) return;
    const delay = retryDelaysMs[Math.min(failures, retryDelaysMs.length - 1)];
    failures++;
    handlers.onIssue(`Terminal stream interrupted; reconnecting (${errorMessage(error)}).`);
    timer = setTimeout(() => { void open(); }, delay);
  }

  async function open() {
    suspend();
    const current = attempt;
    const stale = () => closed || current !== attempt;
    try {
      const screen = await client.terminalScreen(terminalId);
      if (stale()) return;
      if (!incarnation) incarnation = screen.value.runtime_incarnation;
      if (screen.value.runtime_incarnation !== incarnation) throw new Error(TERMINAL_RESTARTED);
      handlers.onScreen(screen.value);
      const result = await withFreshTerminalFence(client, terminalId, incarnation, fence => {
        const id = newActionId();
        return client.terminalAttach({ id, idempotency_key: id, fence, parameters: { target_id: terminalId } });
      });
      const attachment = result.value.terminal_attachment;
      if (!attachment?.stream_capability) throw new Error('The gateway returned no terminal stream.');
      if (stale()) return;
      const opened = await client.terminalStream(terminalId, {
        streamCapability: attachment.stream_capability,
        incarnation: attachment.runtime_incarnation,
        onScreen: next => {
          if (stale()) return;
          if (next.value.runtime_incarnation !== incarnation) { stop(TERMINAL_RESTARTED); return; }
          failures = 0;
          handlers.onIssue('');
          handlers.onScreen(next.value);
        },
        onEnd: error => {
          if (stale()) return;
          stream = undefined;
          retry(error ?? new Error('The terminal stream closed.'));
        },
      });
      if (stale()) opened.close();
      else stream = opened;
    } catch (error) { if (!stale()) retry(error); }
  }

  const unsubscribe = foreground.subscribe(active => {
    if (closed) return;
    if (active) { failures = 0; void open(); }
    else suspend();
  });
  if (foreground.active) void open();
  return { close() { if (!closed) { closed = true; suspend(); unsubscribe(); } } };
}
