import { ClientError, type St3Client } from '@smalltalk/st3-client'
import * as Native from '@smalltalk/st3-client/schema'
import type { TerminalScreen } from '@smalltalk/st3-client/schema'

import { makeActionTerminalInput, type TerminalInputAttempt, type TerminalInputOutcome } from './actionTerminalInput.ts'
import type { TerminalInputPortFactory } from './orderedTerminalInput.ts'
import type { TerminalInputWire } from './terminalInputWire.ts'

const restarted: TerminalInputOutcome = {
  _tag: 'Refused',
  reason: 'The terminal restarted or stopped updating. Queued keys were dropped.',
}
const ungranted: TerminalInputOutcome = { _tag: 'Refused', reason: 'This device is not allowed to type into terminals.' }
const unconfirmed: TerminalInputOutcome = {
  _tag: 'Refused',
  reason: 'The gateway state could not be confirmed. Queued keys were dropped.',
}
// What a submission whose session already ended answers; the session ignores it.
const abandoned: TerminalInputOutcome = { _tag: 'Refused', reason: 'Input is off. Start a new input session to type again.' }

/** The transport refused to start a write because its admission check failed at the last moment. */
class TerminalInputNotPosted extends Error {
  constructor(readonly outcome: TerminalInputOutcome) {
    super('Terminal input was not posted')
  }
}

/**
 * `terminal.input` through a gateway action client built for one input session. Each chunk reads
 * its fences at submission: the freshest held store snapshot, the session's own incarnation and
 * the live screen's sequence. The client's transport admits each action POST synchronously before
 * it calls the real fetch: the chunk's session must still be live, the device must still hold the
 * grant and the shown terminal must still be the bound live incarnation. Only then is the chunk
 * marked posted, so a close, switch, hidden page or lost grant at any earlier point posts nothing.
 */
export const gatewayTerminalInput = ({
  connect,
  transport,
  snapshot,
  liveScreen,
  granted,
}: {
  /** The gateway action client over the given fetch, configured like every other client. */
  readonly connect: (fetchImpl: typeof fetch) => St3Client
  /** The fetch the client would otherwise use. */
  readonly transport: typeof fetch
  /** The freshest snapshot id this client holds, or undefined when none can be obtained. */
  readonly snapshot: () => Promise<string | undefined>
  /** The terminal's currently observed live screen, or undefined when stale or not observed. */
  readonly liveScreen: (terminalRef: string) => TerminalScreen | undefined
  /** Whether this device currently holds the terminal input grant. */
  readonly granted: () => boolean
}): TerminalInputPortFactory =>
  ({ terminalRef, incarnation, registry }) => {
    const admitted = (attempt: TerminalInputAttempt): { readonly screen: TerminalScreen } | TerminalInputOutcome => {
      if (!attempt.live()) return abandoned
      if (!granted()) return ungranted
      const screen = liveScreen(terminalRef)
      return screen?.terminal_id === terminalRef && screen.runtime_incarnation === incarnation ? { screen } : restarted
    }
    // One action is in flight per session, so the transport holds at most the one it may post.
    let armed: TerminalInputAttempt | undefined
    const client = connect((input, init) => {
      if (init?.method !== 'POST') return transport(input, init)
      const attempt = armed
      armed = undefined
      if (attempt === undefined) return Promise.reject(new TerminalInputNotPosted(abandoned))
      const admission = admitted(attempt)
      if ('_tag' in admission) return Promise.reject(new TerminalInputNotPosted(admission))
      attempt.posted()
      return transport(input, init)
    })
    return Promise.resolve(
      makeActionTerminalInput({
        registry,
        submit: async (wire: TerminalInputWire, attempt): Promise<TerminalInputOutcome> => {
          const before = admitted(attempt)
          if ('_tag' in before) return before
          const snapshotId = await snapshot()
          if (snapshotId === undefined) return unconfirmed
          // The fence reads the screen as it is now; the transport admits the post again.
          const current = admitted(attempt)
          if ('_tag' in current) return current
          const id = `action/${crypto.randomUUID()}`
          armed = attempt
          try {
            const response = await client.terminalInput({
              id,
              idempotency_key: id,
              fence: {
                snapshot_id: snapshotId,
                subject_revisions: {},
                runtime_incarnation: incarnation,
                terminal_sequence: current.screen.next_sequence,
              },
              parameters: { terminal_id: terminalRef, mode: wire.mode, value: wire.value },
            })
            const result = Native.decodeUnknownSync(Native.ActionResult)(response.value)
            return result.status === 'rejected'
              ? { _tag: 'Refused', reason: 'The gateway rejected this input. Queued keys were dropped.' }
              : { _tag: 'Delivered' }
          } catch (error) {
            if (error instanceof TerminalInputNotPosted) return error.outcome
            // Discovery failed before the post: nothing was written.
            if (armed === attempt) return unconfirmed
            // No envelope, or a server-side failure after admission: the write may have happened.
            if (!(error instanceof ClientError) || error.status >= 500)
              return { _tag: 'Uncertain', reason: 'Input delivery could not be confirmed. Queued keys were dropped.' }
            if (error.response.code === 'stale-fence')
              return { _tag: 'Refused', reason: 'The terminal changed before this input arrived. Queued keys were dropped.' }
            if (error.status === 401 || error.status === 403)
              return { _tag: 'Refused', reason: 'This device is not allowed to type into this terminal.' }
            return { _tag: 'Refused', reason: 'The gateway refused this input. Queued keys were dropped.' }
          } finally {
            if (armed === attempt) armed = undefined
          }
        },
      }),
    )
  }
