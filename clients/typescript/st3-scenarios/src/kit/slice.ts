import type {
  Agent,
  Attention,
  Capabilities,
  ErrorEnvelope,
  Id,
  Machine,
  Message,
  Mission,
  Resource,
  Runtime,
  SyncPeer,
  TerminalScreen,
  TimelineEntry,
  Work,
} from '@smalltalk/st3-client'

import type { Asciicast } from './asciicast.ts'
import type { SyncStatus } from './syncStatus.ts'

/** A future resource kept encoded, never disguised as a known resource kind. */
export interface RawResource {
  readonly id: Id
  readonly kind: string
  readonly revision: string
  readonly updated_at: string
  readonly [field: string]: unknown
}
export type WireResource = Resource | RawResource

/** Exact contaminated pointer, with its strict-valid replacement; omission removes an extra key. */
export interface UnknownPath {
  readonly pointer: string
  readonly known_value?: unknown
}

export const SLICE_KINDS = ['roster', 'details', 'attention', 'conversation', 'terminal', 'sync'] as const
export type SliceKind = (typeof SLICE_KINDS)[number]

export interface RosterState {
  readonly resources?: RawResource[]
  readonly agents: Agent[]
  readonly runtimes: Runtime[]
  readonly machines: Machine[]
  /** Agent display order. */
  readonly order: Id[]
}

export interface DetailsState {
  readonly resources?: RawResource[]
  readonly missions: Mission[]
  readonly work: Work[]
}

export interface AttentionState {
  readonly resources?: RawResource[]
  readonly attention: Attention[]
  readonly messages: Message[]
}

/** Portable description of the uncommitted prefix; only TypeScript replay synthesizes it. */
export interface ConversationHistory {
  readonly kind: 'seeded-turns'
  readonly seed: number
  readonly world: string
  readonly total_turns: number
  readonly total_entries: number
  readonly committed_from_sequence: number
  /** Older-page cursor immediately beyond the committed entries. */
  readonly next_cursor: string
}

export interface ConversationThread {
  readonly agent: Id
  readonly session_id: Id
  /** Committed entries, oldest first; the newest `page_size` form the live window. */
  readonly items: TimelineEntry[]
  readonly page_size: number
  readonly has_more: boolean
  readonly history?: ConversationHistory
}

export interface ConversationState {
  readonly threads: ConversationThread[]
}

export interface TerminalRecord {
  readonly terminal: Id
  /** Agent that owns the terminal. */
  readonly owner: Id
  /** The terminal runtime (`runtime_kind: terminal`); `runtimesGet` reads it before every attach. */
  readonly runtime: Runtime
  readonly incarnation: string
  readonly cast: Asciicast
  /** Screens a client sees; `at_ms <= 0` are in the past, the last of them is current at `now`. */
  readonly screens: { readonly at_ms: number; readonly screen: TerminalScreen }[]
}

export interface TerminalState {
  readonly terminals: TerminalRecord[]
}

/**
 * Surface whose status the SDK reports: a collection (`agents`), or a selector-qualified stream
 * (`conversation:<agent>`, `terminal:<terminal>`), or an HTTP-only read (`read:<route>`).
 */
export type SyncSurface = string

/** Portable `SyncStatus` with every instant given as an offset (ms) from `now`. */
export interface SyncExpectation {
  readonly surface: SyncSurface
  /** Clock offset at which the status holds. */
  readonly at_ms: number
  readonly status: SyncStatus
  /**
   * `exact` compares the whole status. `shape` compares tags, server codes and messages, and
   * local kinds and caps; it ignores attempts, instants and issue texts the SDK chooses.
   */
  readonly compare: 'exact' | 'shape'
}

export interface SyncState {
  /** The `GET capabilities` envelope's value. */
  readonly capabilities: Capabilities
  /** A local condition the consumer test must create; replay cannot. */
  readonly local?: { readonly _tag: 'visible-follows'; readonly count: number } | { readonly _tag: 'hidden-follow'; readonly hidden: string; readonly visible: string[] }
  readonly expected: SyncExpectation[]
}

export interface SliceStates {
  readonly roster: RosterState
  readonly details: DetailsState
  readonly attention: AttentionState
  readonly conversation: ConversationState
  readonly terminal: TerminalState
  readonly sync: SyncState
}

/** Names one subscription by collection and filter. */
export type Selector =
  | { readonly collection: 'agents' | 'missions' | 'work' | 'attention' }
  | { readonly collection: 'conversation'; readonly conversation: Id }
  | { readonly collection: 'terminal'; readonly terminal: Id }

export type HttpRoute = 'capabilities' | 'resources' | 'events' | 'timeline' | 'actions' | (string & {})

interface At {
  readonly at_ms: number
  /** Store version after this event. */
  readonly store: number
}

export type ChangesEvent = At & {
  readonly _tag: 'changes'
  readonly upserts: WireResource[]
  readonly removes: Id[]
  readonly order?: Id[]
}

export type ConversationEvent =
  | (At & { readonly _tag: 'thread-create'; readonly thread: ConversationThread })
  | (At & { readonly _tag: 'thread-remove'; readonly agent: Id })
  | (At & { readonly _tag: 'entries'; readonly agent: Id; readonly items: TimelineEntry[] })
  | (At & {
      readonly _tag: 'replace'
      readonly agent: Id
      readonly session_id: Id
      readonly items: TimelineEntry[]
      readonly has_more: boolean
    })

export type TerminalEvent =
  | (At & { readonly _tag: 'terminal-create'; readonly record: TerminalRecord })
  | (At & { readonly _tag: 'terminal-remove'; readonly terminal: Id })
  | (At & { readonly _tag: 'screen'; readonly terminal: Id; readonly screen: TerminalScreen })
  | (At & { readonly _tag: 'unavailable'; readonly terminal: Id })
  | (At & { readonly _tag: 'end'; readonly terminal: Id })
  | (At & { readonly _tag: 'incarnation'; readonly terminal: Id; readonly incarnation: string })

/** All named query keys must match; `cursor: 'present'` targets only older pages. */
export interface HttpCondition {
  readonly cursor?: 'present' | 'absent'
  readonly query?: Readonly<Record<string, string>>
}

export type SyncEvent =
  | (At & { readonly _tag: 'open-fail'; readonly opens?: number | 'all' })
  | (At & { readonly _tag: 'open-ok' })
  | (At & { readonly _tag: 'open-hold' })
  | (At & { readonly _tag: 'open-release' })
  | (At & {
      readonly _tag: 'http-raw'
      readonly route: HttpRoute
      readonly when?: HttpCondition
      readonly status: number
      readonly content_type: string
      readonly body: string
    })
  | (At & { readonly _tag: 'close'; readonly code: number; readonly reason: string })
  | (At & { readonly _tag: 'reopen'; readonly after_ms: number })
  | (At & { readonly _tag: 'http-error'; readonly route: HttpRoute; readonly when?: HttpCondition; readonly status: number; readonly envelope: ErrorEnvelope })
  | (At & { readonly _tag: 'http-ok'; readonly route: HttpRoute; readonly when?: HttpCondition })
  | (At & { readonly _tag: 'hold'; readonly selector: Selector })
  | (At & { readonly _tag: 'release'; readonly selector: Selector })
  | (At & { readonly _tag: 'resync'; readonly selector: Selector; readonly code?: string; readonly message?: string })
  | (At & {
      readonly _tag: 'error'
      readonly selector?: Selector
      readonly code?: string
      readonly message: string
      readonly retryable: boolean
      /** Reemit on each matching subscribe until `error-clear`. */
      readonly repeat?: boolean
    })
  | (At & { readonly _tag: 'notice'; readonly peers: SyncPeer[] })
  | (At & { readonly _tag: 'error-clear'; readonly selector?: Selector })
  | (At & { readonly _tag: 'notice-clear' })

export interface SliceEvents {
  readonly roster: ChangesEvent
  readonly details: ChangesEvent
  readonly attention: ChangesEvent
  readonly conversation: ConversationEvent
  readonly terminal: TerminalEvent
  readonly sync: SyncEvent
}

export type TimelineEvent = SliceEvents[SliceKind]

export type SliceSource = { readonly _tag: 'synthetic'; readonly seed: number } | { readonly _tag: 'recorded'; readonly recording: string }

/** One slice of one world: the state at `now` plus later frame-shaped events. */
export interface Slice<K extends SliceKind = SliceKind> {
  readonly kind: K
  readonly variant: string
  readonly source: SliceSource
  readonly decode: 'strict' | 'tolerant'
  readonly unknown?: readonly UnknownPath[]
  /** Every first reply for this slice's subscriptions is withheld (the `loading` variants). */
  readonly loading: boolean
  readonly state: SliceStates[K]
  readonly timeline: SliceEvents[K][]
}

export type AnySlice = { [K in SliceKind]: Slice<K> }[SliceKind]
