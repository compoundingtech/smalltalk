import * as Atom from 'effect/reactivity/Atom'
import type * as AtomRegistry from 'effect/reactivity/AtomRegistry'

import type { TerminalInputPort, TerminalInputState } from './orderedTerminalInput.ts'
import { pasteMaxBytes, rawInput, terminalInputWire, type TerminalInputWire } from './terminalInputWire.ts'

/** What one `terminal.input` action did. Every reason is fixed, user-readable copy. */
export type TerminalInputOutcome =
  | { readonly _tag: 'Delivered' }
  /** The gateway answered and did not write the input. */
  | { readonly _tag: 'Refused'; readonly reason: string }
  /** No answer arrived; the input may or may not have reached the terminal. */
  | { readonly _tag: 'Uncertain'; readonly reason: string }

/**
 * One submission's tie to its session. `live` turns false synchronously when the session closes
 * or stops; a submit checks it after every await and immediately before it posts, and calls
 * `posted` once the write may have reached the gateway.
 */
export interface TerminalInputAttempt {
  readonly live: () => boolean
  readonly posted: () => void
}

/** Submits one chunk with fences read fresh for this call. Never rejects. */
export type TerminalInputSubmit = (wire: TerminalInputWire, attempt: TerminalInputAttempt) => Promise<TerminalInputOutcome>

/** A send the port refused locally without touching the session. */
export class TerminalInputUnsendable extends Error {}

type Chunk =
  | { readonly mode: 'key'; readonly value: string; readonly waiters: Waiter[]; posted?: boolean }
  | { readonly mode: 'raw'; bytes: Uint8Array; readonly waiters: Waiter[]; posted?: boolean }
interface Waiter {
  readonly resolve: () => void
  readonly reject: (reason: Error) => void
}

const concat = (left: Uint8Array, right: Uint8Array) => {
  const joined = new Uint8Array(left.length + right.length)
  joined.set(left)
  joined.set(right, left.length)
  return joined
}

/**
 * One input session for one terminal incarnation over HTTP actions: one action in flight, FIFO,
 * no replay. Raw chunks waiting behind the in-flight action merge into one write, which keeps
 * their bytes and order; a named key never merges, so Escape cannot fuse with the next key.
 * The first refusal or uncertain delivery ends the session and drops everything still queued.
 */
export const makeActionTerminalInput = ({
  registry,
  submit,
}: {
  readonly registry: AtomRegistry.AtomRegistry
  readonly submit: TerminalInputSubmit
}): TerminalInputPort => {
  const state = Atom.make<TerminalInputState>({ _tag: 'Idle' }).pipe(Atom.keepAlive)
  let phase: 'idle' | 'ready' | 'closed' = 'idle'
  let sent = 0
  let inFlight: Chunk | undefined
  const queue: Chunk[] = []
  const waiting = () => (inFlight === undefined ? 0 : 1) + queue.length
  const publish = () => {
    if (phase === 'ready') registry.set(state, { _tag: 'Ready', nextSeq: sent, pending: waiting() })
  }
  const stop = (reason: string, uncertain: boolean) => {
    if (phase === 'closed') return
    phase = 'closed'
    registry.set(state, { _tag: 'Closed', reason, uncertain })
    const dropped = [...(inFlight?.waiters ?? []), ...queue.flatMap((chunk) => chunk.waiters)]
    inFlight = undefined
    queue.length = 0
    for (const waiter of dropped) waiter.reject(new Error(reason))
  }
  const pump = () => {
    if (phase !== 'ready' || inFlight !== undefined) return
    const next = queue.shift()
    if (next === undefined) return
    inFlight = next
    sent += 1
    publish()
    const wire: TerminalInputWire = next.mode === 'key' ? { mode: 'key', value: next.value } : rawInput(next.bytes)
    const attempt: TerminalInputAttempt = {
      live: () => phase === 'ready' && inFlight === next,
      posted: () => {
        next.posted = true
      },
    }
    void submit(wire, attempt)
      .catch((): TerminalInputOutcome => ({
        _tag: 'Uncertain',
        reason: 'Input delivery could not be confirmed. Queued keys were dropped.',
      }))
      .then((outcome) => {
        // A late answer for a closed session settles nothing: its waiters were already rejected.
        if (inFlight !== next) return
        if (outcome._tag !== 'Delivered') {
          stop(outcome.reason, outcome._tag === 'Uncertain')
          return
        }
        inFlight = undefined
        for (const waiter of next.waiters) waiter.resolve()
        publish()
        pump()
      })
  }
  return {
    state,
    open: () => {
      if (phase !== 'idle') return
      phase = 'ready'
      publish()
    },
    send: (bytes) => {
      if (phase !== 'ready') return Promise.reject(new Error('Terminal input is not open'))
      if (bytes.length === 0) return Promise.resolve()
      if (bytes.length > pasteMaxBytes) return Promise.reject(new TerminalInputUnsendable('Input is too large'))
      const wire = terminalInputWire(bytes)
      if (wire === undefined) return Promise.reject(new TerminalInputUnsendable('Input holds a NUL byte'))
      const waiter = Promise.withResolvers<void>()
      const tail = queue.at(-1)
      if (wire.mode === 'raw' && tail?.mode === 'raw' && tail.bytes.length + bytes.length <= pasteMaxBytes) {
        tail.bytes = concat(tail.bytes, bytes)
        tail.waiters.push(waiter)
      } else if (wire.mode === 'raw') queue.push({ mode: 'raw', bytes, waiters: [waiter] })
      else queue.push({ mode: 'key', value: wire.value, waiters: [waiter] })
      publish()
      pump()
      return waiter.promise
    },
    // Closing cannot recall a write already posted; say so rather than claim nothing was sent.
    close: () =>
      inFlight?.posted === true
        ? stop('The last input may have reached the terminal; it was not sent again.', true)
        : stop('Input is off. Start a new input session to type again.', false),
  }
}
