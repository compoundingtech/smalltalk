import * as Atom from 'effect/reactivity/Atom'

import type { HistoryState } from './history.ts'

/** Retained scrollback projection with optional explicit fetch actions. */
export interface TerminalHistoryPort {
  readonly state: Atom.Atom<HistoryState>
  readonly loadOlder?: () => void
  readonly refresh?: () => void
}
/** Parent selects a fresh port for each observed terminal/runtime incarnation. */
export type TerminalHistoryFactory = (terminal: string, incarnation: string) => TerminalHistoryPort

/** Unsupported-producer history state shared by every unavailable port. */
export const historyUnavailable: HistoryState = {
  _tag: 'Issue',
  issue: {
    _tag: 'Unavailable',
    reason: 'unsupported',
    detail:
      'The installed producer client does not publish terminal history yet. The current screen is still available.',
  },
}

/** No HTTP endpoint guessing: replaced by the generated adapter only after publication admission. */
const unavailablePort: TerminalHistoryPort = { state: Atom.make(historyUnavailable) }
/** Adapter used until the generated client publishes real retained history. */
export const unavailableTerminalHistory: TerminalHistoryFactory = () => unavailablePort
