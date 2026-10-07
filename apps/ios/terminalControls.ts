import type { St3Client } from '../../clients/typescript/st3-client';
import type { Fence, TerminalScreen } from '../../clients/typescript/st3-client/Models.generated';

const FENCE_ATTEMPTS = 8;

export type TerminalFence = Fence & Required<Pick<Fence, 'runtime_incarnation' | 'terminal_sequence'>>;

function isStaleFence(error: unknown): boolean {
  return !!error && typeof error === 'object' && 'response' in error
    && !!error.response && typeof error.response === 'object'
    && 'code' in error.response && error.response.code === 'stale-fence';
}

/** An st that predates unfenced typing refuses a key without the screen's sequence, and says so. */
export function wantsScreenSequence(error: unknown): boolean {
  const message = !!error && typeof error === 'object' && 'response' in error && !!error.response && typeof error.response === 'object' && 'message' in error.response
    ? String(error.response.message) : '';
  return message.includes('requires a sequence fence');
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
  // st fences terminal actions to its whole store's index, which a busy host moves several
  // times a second; a fresh read and a quick resend usually lands.
  for (let attempt = 0; attempt < FENCE_ATTEMPTS; attempt++) {
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
    catch (error) { if (!isStaleFence(error) || attempt === FENCE_ATTEMPTS - 1) throw error; }
  }
  throw new Error('Terminal changed too often; try again.');
}

export const TERMINAL_RESTARTED = 'Terminal restarted; reopen it before sending input.';
export type TerminalFollowHandlers = {
  /** Each screen replaces the one before it. */
  onScreen: (screen: TerminalScreen) => void;
  /** A problem to show; an empty string clears it. */
  onIssue: (issue: string) => void;
};

/** The app's foreground state, such as `ForegroundGate`. */
export type Foreground = {
  readonly active: boolean;
  subscribe(listener: (active: boolean) => void): () => void;
};
