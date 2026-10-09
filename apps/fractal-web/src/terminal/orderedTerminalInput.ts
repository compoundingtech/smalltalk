import * as Atom from 'effect/reactivity/Atom'
import type * as AtomRegistry from 'effect/reactivity/AtomRegistry'

/** Lifetime of one ordered input session, from explicit arming to close. */
export type TerminalInputState =
  | { readonly _tag: 'Idle' }
  | { readonly _tag: 'Opening' }
  | { readonly _tag: 'Ready'; readonly nextSeq: number; readonly pending: number }
  | { readonly _tag: 'Closed'; readonly reason: string; readonly uncertain: boolean }

/** A single admitted terminal input session owned by the binding that armed it. */
export interface TerminalInputPort {
  readonly state: Atom.Atom<TerminalInputState>
  readonly open: () => void
  /** Transport acknowledgement is not application/PTY consumption. No uncertain byte replay. */
  readonly send: (bytes: Uint8Array) => Promise<void>
  readonly close: () => void
}

/** Absence of DataSource.terminalInput means the actual producer does not support ordered input. */
export type TerminalInputPortFactory = (options: {
  readonly terminalRef: string
  readonly incarnation: string
  readonly registry: AtomRegistry.AtomRegistry
}) => Promise<TerminalInputPort>

/** Internal behavior boundary, not a copy of the D18 wire schema. The generated bridge owns IDs. */
export interface TerminalInputTransport {
  readonly holdFollow: () => void
  readonly open: () => void
  readonly send: (batch: { readonly seq: number; readonly bytes: Uint8Array }) => void
  readonly close: () => void
  readonly releaseFollow: () => void
}

/** Port plus the transport callbacks the generated bridge drives. */
export interface OrderedTerminalInput extends TerminalInputPort {
  readonly opened: (nextSeq: number) => void
  readonly acknowledged: (seq: number) => void
  /** Disconnect, ownership/incarnation change, follow loss or grant revocation ends this lifetime. */
  readonly invalidate: (reason: string) => void
}

/** One socket lifetime, explicit arming, held follow, ordered transport ACKs and no replays. */
export const makeOrderedTerminalInput = ({
  registry,
  transport,
  localOwner,
  controlGranted,
}: {
  readonly registry: AtomRegistry.AtomRegistry
  readonly transport: TerminalInputTransport
  readonly localOwner: boolean
  readonly controlGranted: boolean
}): OrderedTerminalInput => {
  const state = Atom.make<TerminalInputState>({ _tag: 'Idle' }).pipe(Atom.keepAlive)
  const pending = new Map<number, { resolve: () => void; reject: (reason: Error) => void }>()
  let nextSeq: number | undefined
  let phase: 'idle' | 'opening' | 'ready' | 'closed' = 'idle'
  let held = false
  const stop = (reason: string) => {
    if (phase === 'closed') return
    phase = 'closed'
    const uncertain = pending.size > 0
    registry.set(state, { _tag: 'Closed', reason, uncertain })
    for (const batch of pending.values())
      batch.reject(
        new Error(
          `${reason}${uncertain ? '; input delivery is uncertain and will not be replayed' : ''}`,
        ),
      )
    pending.clear()
    if (held) {
      try {
        transport.close()
      } catch {
        /* Dead socket: never retry or reopen. */
      }
      try {
        transport.releaseFollow()
      } catch {
        /* Dead socket already released its follows. */
      }
    }
    held = false
  }
  return {
    state,
    open: () => {
      if (phase !== 'idle') return
      if (!localOwner || !controlGranted) {
        stop(
          !localOwner
            ? 'Input is unavailable for a terminal owned by another host'
            : 'This device lacks terminal.control',
        )
        return
      }
      phase = 'opening'
      registry.set(state, { _tag: 'Opening' })
      try {
        transport.holdFollow()
        held = true
        transport.open()
      } catch {
        stop('The input socket could not be opened; open a fresh session')
      }
    },
    send: (bytes) => {
      if (phase !== 'ready' || nextSeq === undefined)
        return Promise.reject(new Error('Terminal input is not open'))
      if (bytes.length === 0) return Promise.resolve()
      const seq = nextSeq
      if (!Number.isSafeInteger(seq) || seq >= Number.MAX_SAFE_INTEGER) {
        stop('The input sequence limit was reached; open a fresh session')
        return Promise.reject(new Error('Terminal input sequence exhausted'))
      }
      nextSeq = seq + 1
      const result = new Promise<void>((resolve, reject) => {
        pending.set(seq, { resolve, reject })
      })
      registry.set(state, { _tag: 'Ready', nextSeq, pending: pending.size })
      try {
        transport.send({ seq, bytes })
      } catch {
        stop('The input socket disconnected during send')
      }
      return result
    },
    opened: (sequence) => {
      if (phase === 'closed' || phase === 'idle') return
      if (phase !== 'opening' || !Number.isSafeInteger(sequence) || sequence < 0) {
        stop('The server returned an invalid input session')
        return
      }
      nextSeq = sequence
      phase = 'ready'
      registry.set(state, { _tag: 'Ready', nextSeq, pending: 0 })
    },
    acknowledged: (seq) => {
      if (phase !== 'ready') return
      const batch = pending.get(seq)
      if (batch === undefined) return
      if (pending.keys().next().value !== seq) {
        stop('The server acknowledged input out of order')
        return
      }
      pending.delete(seq)
      batch.resolve()
      if (nextSeq !== undefined)
        registry.set(state, { _tag: 'Ready', nextSeq, pending: pending.size })
    },
    invalidate: stop,
    close: () => stop('Input session closed'),
  }
}
