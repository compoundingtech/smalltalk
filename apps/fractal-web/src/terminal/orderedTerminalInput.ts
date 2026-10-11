import type * as Atom from 'effect/reactivity/Atom'
import type * as AtomRegistry from 'effect/reactivity/AtomRegistry'

/** Lifetime of one ordered input session, from explicit arming to close. */
export type TerminalInputState =
  | { readonly _tag: 'Idle' }
  | { readonly _tag: 'Opening' }
  /** `nextSeq` counts chunks submitted so far; `pending` counts chunks not yet confirmed. */
  | { readonly _tag: 'Ready'; readonly nextSeq: number; readonly pending: number }
  /** `reason` is fixed, user-readable copy. */
  | { readonly _tag: 'Closed'; readonly reason: string; readonly uncertain: boolean }

/** A single admitted terminal input session owned by the binding that armed it. */
export interface TerminalInputPort {
  readonly state: Atom.Atom<TerminalInputState>
  readonly open: () => void
  /** Gateway acknowledgement is not application/PTY consumption. No uncertain byte replay. */
  readonly send: (bytes: Uint8Array) => Promise<void>
  readonly close: () => void
}

/** Absence of DataSource.terminalInput means the actual producer does not support ordered input. */
export type TerminalInputPortFactory = (options: {
  /** The subject address whose terminal feed the view shows (`terminal/<agent path>`). */
  readonly subject: string
  /** The gateway's id for that terminal (`terminal/<agent id>`); a different string from `subject`. */
  readonly terminalRef: string
  readonly incarnation: string
  readonly registry: AtomRegistry.AtomRegistry
}) => Promise<TerminalInputPort>
