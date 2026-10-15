/**
 * Which terminal path a pane shows, who receives what is typed, the chip that names it, and the
 * grid an element can hold. Output comes from the raw PTY stream (ATTACH or view-only PEEK) and
 * falls back to the projected screen when the stream is refused, keeps failing, or the source does
 * not offer it. Typing goes over a raw ATTACH when this device may control the terminal, else
 * through the gateway's `terminal.input` actions, else nowhere.
 */

import {
  ptyFallback,
  type PtyMode,
  type PtyOutput,
  type PtySession,
  type PtySize,
  type PtyState,
  type PtyTarget,
} from '@st3/sdk/effect'
import type * as Effect from 'effect/Effect'
import type * as Scope from 'effect/Scope'

import type { Grants } from '../data/source.ts'

/**
 * A source's raw PTY stream: one scoped session per pane, bound to one terminal incarnation.
 * Closing the scope detaches. Absent when the source has no raw stream.
 */
export interface RawTerminalPort {
  readonly open: (options: {
    readonly target: PtyTarget
    readonly mode: PtyMode
    readonly size: PtySize
    readonly onOutput: (output: PtyOutput) => void
  }) => Effect.Effect<PtySession, never, Scope.Scope>
}

/** Who receives what is typed into the pane. */
export type TerminalInputPath =
  /** Keys leave as DATA on the raw ATTACH socket, and the element's size is this client's vote. */
  | { readonly _tag: 'Attach' }
  /** Keys leave as `terminal.input` actions; the shared size changes only by explicit resize. */
  | { readonly _tag: 'Gateway' }
  /** Nothing typed reaches the terminal. */
  | { readonly _tag: 'ReadOnly'; readonly reason: string }

/**
 * The pane's one input decision. A raw ATTACH needs `terminal.control` and a stream that has not
 * already fallen back for this terminal; without it, a device holding `terminal.input` keeps typing
 * through the gateway, as it could before the raw stream existed.
 *
 * On the pinned server `terminal.input` is itself gated by the `terminal.control` scope, so the
 * Gateway path is reached by a device holding both after its ATTACH was refused or lost. A limited
 * pairing holds neither and reads PEEK only, until the server admits `terminal.control` for it
 * (compoundingtech/smalltalk#2873).
 */
export const terminalInputPath = ({
  raw,
  gateway,
  grants,
  attachLost,
}: {
  /** The source offers the raw PTY stream. */
  readonly raw: boolean
  /** The source offers gateway `terminal.input` sessions. */
  readonly gateway: boolean
  readonly grants: Pick<Grants, 'terminalInput' | 'terminalControl'>
  /** A raw ATTACH for this terminal was refused, kept failing, or its emulator could not start. */
  readonly attachLost: boolean
}): TerminalInputPath => {
  if (raw && grants.terminalControl === 'granted' && !attachLost) return { _tag: 'Attach' }
  if (gateway && grants.terminalInput === 'granted') return { _tag: 'Gateway' }
  return {
    _tag: 'ReadOnly',
    reason:
      grants.terminalInput === 'granted' || grants.terminalControl === 'granted'
        ? 'This gateway does not accept typing into this terminal.'
        : 'This device may watch this terminal but not type into it.',
  }
}

/** The raw stream mode an input path needs: only an ATTACH may type on the socket or vote a size. */
export const rawModeFor = (input: TerminalInputPath): PtyMode => (input._tag === 'Attach' ? 'attach' : 'peek')

/** The pane's output path. */
export type TerminalPresentation =
  /** The raw session owns the screen. */
  | { readonly _tag: 'Raw' }
  /** The projected screen is shown, with the reason the raw stream is not. */
  | { readonly _tag: 'Projected'; readonly reason: string }

/** What the raw path can say about itself. */
export type RawPreparation =
  | { readonly _tag: 'Unsupported' }
  | { readonly _tag: 'Preparing' }
  | { readonly _tag: 'Unprepared'; readonly message: string }
  | { readonly _tag: 'Session'; readonly state: PtyState }

/** Fallback selection: everything that is not a usable raw session reads the projected screen. */
export const terminalPresentation = (raw: RawPreparation): TerminalPresentation => {
  if (raw._tag === 'Unsupported')
    return { _tag: 'Projected', reason: 'This gateway does not offer a live terminal stream.' }
  if (raw._tag === 'Unprepared') return { _tag: 'Projected', reason: raw.message }
  if (raw._tag === 'Preparing') return { _tag: 'Raw' }
  const fallback = ptyFallback(raw.state)
  if (fallback === undefined) return { _tag: 'Raw' }
  return {
    _tag: 'Projected',
    reason:
      fallback.reason === 'refused'
        ? `The live terminal stream was refused (${fallback.message}). Showing the projected screen.`
        : `The live terminal stream failed (${fallback.message}). Showing the projected screen.`,
  }
}

/** The pane's state chip. */
export type TerminalChip = { readonly label: string; readonly tone: 'live' | 'pending' | 'readonly' | 'ended' }

export const terminalChip = ({
  raw,
  presentation,
  input,
  typing,
}: {
  readonly raw: RawPreparation
  readonly presentation: TerminalPresentation
  readonly input: TerminalInputPath
  readonly typing: boolean
}): TerminalChip => {
  if (presentation._tag === 'Raw') {
    if (raw._tag !== 'Session') return { label: 'Connecting…', tone: 'pending' }
    switch (raw.state._tag) {
      case 'Live':
        break
      case 'Connecting':
        if (raw.state.attempt === 1) return { label: 'Connecting…', tone: 'pending' }
        return { label: 'Reconnecting…', tone: 'pending' }
      case 'Reconnecting':
      case 'Stale':
        return { label: 'Reconnecting…', tone: 'pending' }
      case 'Exited':
        return { label: `Exited (${raw.state.code})`, tone: 'ended' }
      default:
        return { label: 'Closed', tone: 'ended' }
    }
  }
  if (input._tag === 'ReadOnly') return { label: 'Read-only', tone: 'readonly' }
  return typing ? { label: 'Typing', tone: 'live' } : { label: 'Click to type', tone: 'live' }
}

/**
 * The grid that fits an element's content box. `undefined` until the font has a measured cell,
 * so an unmeasured element never shrinks the shared PTY.
 */
export const gridFor = ({
  width,
  height,
  cellWidth,
  cellHeight,
}: {
  readonly width: number
  readonly height: number
  readonly cellWidth: number
  readonly cellHeight: number
}): PtySize | undefined => {
  if (!(cellWidth > 0) || !(cellHeight > 0) || !(width > 0) || !(height > 0)) return undefined
  return {
    cols: Math.max(2, Math.min(65535, Math.floor(width / cellWidth))),
    rows: Math.max(1, Math.min(65535, Math.floor(height / cellHeight))),
  }
}

/** Element resizes settle for this long before one RESIZE frame is sent. */
export const resizeSettleMillis = 120

/** A pending RESIZE frame that settles after the element stops changing size. */
export interface ResizeDebouncer {
  readonly observe: (size: PtySize) => void
  readonly dispose: () => void
}

/**
 * Debounce element sizes into RESIZE frames: only the last size of a burst is sent, and a size
 * equal to the last one sent is not sent again. The server applies min-wins over writable clients.
 */
export const makeResizeDebouncer = ({
  send,
  settleMillis = resizeSettleMillis,
  initial,
}: {
  readonly send: (size: PtySize) => void
  readonly settleMillis?: number
  /** The size the session attached with. */
  readonly initial?: PtySize | undefined
}): ResizeDebouncer => {
  let sent = initial
  let pending: PtySize | undefined
  let timer: ReturnType<typeof setTimeout> | undefined
  const flush = () => {
    timer = undefined
    const size = pending
    pending = undefined
    if (size === undefined || (sent !== undefined && sent.cols === size.cols && sent.rows === size.rows)) return
    sent = size
    send(size)
  }
  return {
    observe: (size: PtySize) => {
      pending = size
      clearTimeout(timer)
      timer = setTimeout(flush, settleMillis)
    },
    dispose: () => {
      clearTimeout(timer)
      pending = undefined
    },
  }
}
