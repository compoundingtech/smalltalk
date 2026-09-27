import type { St3Client } from '../../clients/typescript/st3-client';
import type { Fence } from '../../clients/typescript/st3-client/Models.generated';

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
