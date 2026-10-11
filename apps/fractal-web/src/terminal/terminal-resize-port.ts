import { ClientError, type St3Client } from '@smalltalk/st3-client'
import * as Native from '@smalltalk/st3-client/schema'

/** One explicit resize attempt: confirmed geometry or a user-readable refusal. */
export type TerminalResizeResult =
  | { readonly _tag: 'Requested'; readonly columns: number; readonly rows: number }
  | { readonly _tag: 'Refused'; readonly detail: string }

/** Explicit shared-PTY geometry changes, never browser viewport-driven resizing. */
export interface TerminalResizePort {
  readonly resize: (request: {
    readonly terminalId: string
    readonly incarnation: string
    readonly columns: number
    readonly rows: number
  }) => Promise<TerminalResizeResult>
}

/** One fresh fence and one submission. A stale or uncertain action is never retried automatically. */
export const gatewayTerminalResize = (client: St3Client): TerminalResizePort => ({
  resize: async ({ terminalId, incarnation, columns, rows }) => {
    if (!Number.isSafeInteger(columns) || !Number.isSafeInteger(rows) || columns < 1 || rows < 1) {
      return { _tag: 'Refused', detail: 'Use positive whole-number terminal dimensions.' }
    }
    try {
      const capabilities = Native.decodeUnknownSync(Native.Capabilities)(
        (await client.capabilities()).value,
      )
      if (
        !capabilities.capabilities.some(
          (capability) => capability.id === 'terminal.resize' && capability.state === 'granted',
        )
      ) {
        return {
          _tag: 'Refused',
          detail:
            'This device is not granted terminal resize. The current terminal size is unchanged.',
        }
      }
      const observed = await client.terminalScreen(terminalId)
      const screen = Native.decodeUnknownSync(Native.TerminalScreen)(observed.value)
      const snapshot = Native.decodeUnknownSync(Native.Snapshot)(observed.snapshot)
      if (screen.terminal_id !== terminalId || screen.runtime_incarnation !== incarnation) {
        return {
          _tag: 'Refused',
          detail: 'The terminal restarted. Reopen it before changing its size.',
        }
      }
      const id = `action/${crypto.randomUUID()}`
      const response = await client.terminalResize({
        id,
        idempotency_key: id,
        fence: {
          snapshot_id: snapshot.id,
          subject_revisions: {},
          runtime_incarnation: incarnation,
          terminal_sequence: screen.next_sequence,
        },
        parameters: { terminal_id: terminalId, columns, rows },
      })
      const result = Native.decodeUnknownSync(Native.ActionResult)(response.value)
      return result.status === 'rejected'
        ? {
            _tag: 'Refused',
            detail: 'The gateway rejected the resize. The observed size remains authoritative.',
          }
        : { _tag: 'Requested', columns, rows }
    } catch (error) {
      return {
        _tag: 'Refused',
        detail:
          error instanceof ClientError && error.response.code === 'stale-fence'
            ? 'The terminal changed before resizing. Check its current size and try again.'
            : 'The resize could not be confirmed. Check the observed terminal size before trying again; no request was replayed.',
      }
    }
  },
})
