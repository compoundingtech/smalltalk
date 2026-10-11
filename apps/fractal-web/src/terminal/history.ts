import type { TerminalLine } from '@smalltalk/st3-client/schema'

/** Presentation window, not a producer wire DTO. Only explicit main-buffer history belongs here. */
export interface HistoryWindow {
  readonly terminal: string
  readonly incarnation: string
  readonly columns: number
  readonly lines: readonly TerminalLine[]
  readonly nextBefore: string | undefined
  readonly retainedRows: number
}

/** Why retained history cannot be shown for the current screen. */
export type HistoryIssue =
  | {
      readonly _tag: 'Unavailable'
      readonly reason: 'unsupported' | 'ungranted'
      readonly detail: string
    }
  | { readonly _tag: 'Gap'; readonly detail: string }
  | { readonly _tag: 'AlternateScreen'; readonly detail: string }
  | { readonly _tag: 'IncarnationChanged'; readonly detail: string }
  | { readonly _tag: 'Failed'; readonly detail: string }

/** Loadable history projection with an optional in-flight action. */
export type HistoryState =
  | { readonly _tag: 'Loading' }
  | { readonly _tag: 'Issue'; readonly issue: HistoryIssue }
  | {
      readonly _tag: 'Ready'
      readonly window: HistoryWindow
      readonly activity: 'idle' | 'loading'
      readonly issue?: HistoryIssue
    }
