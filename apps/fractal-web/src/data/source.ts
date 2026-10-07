/**
 * The one seam between wf features and where their data comes from.
 *
 * Features never import fixtures or the SDK; they read projections through the hooks in
 * `react.tsx`. Implementations:
 *
 * - `fixtures` — `fixtureSource({ world, overrides })`: constant atoms over the shared
 *   deterministic world (`src/fixtures/world.ts`, projected in `src/fixtures/projections.ts`).
 *   Synchronous first render, so plays and screenshots are stable.
 * - `live` — retained atoms fed by the SDK's gateway follows.
 *
 * Sources publish generated decoded DTOs. Native wire decoding happens once at the source;
 * registry consumers validate Schema.toType(generatedCodec). Derived summaries/counts/labels
 * are computed from these atoms, so fixture and live adapters share the same contract.
 */
import type {
  Agent as AgentRow,
  Attention,
  Mission,
  TerminalScreen,
} from '@smalltalk/st3-client/schema'
import type { ConnectionState } from '@st3/sdk/effect'
import type { FeedSync } from './feedSync.ts'
import * as Atom from 'effect/reactivity/Atom'

import type { ConversationItem } from '../conversation/model.ts'
import type { ProposedMissionFields } from '../missions/model.ts'
import type { MonitorSource } from '../monitor/source.ts'
import type { AgentResourceSource } from '../resources/agent/source.ts'
import type { SubjectEnvelope } from '../resources/envelope.ts'
import type { TerminalHistoryFactory } from '../terminal/historySource.ts'
import type { TerminalResizePort } from '../terminal/terminal-resize-port.ts'
import type { SubjectReads } from './subjectReadPort.ts'

/**
 * One followed projection as a feature sees it. Fixtures can produce every case (what `AllStates`
 * pins); live derives it from the SDK's `Observed<A>` (`value` + `freshness`) and follow errors.
 */
export type Feed<A> =
  | { readonly _tag: 'Waiting' }
  | {
      readonly _tag: 'Observed'
      readonly value: A
      readonly freshness: 'live' | 'stale'
      /** A newer transcript read failed; these are still the last trusted rows, not a repair. */
      readonly error?: {
        readonly reason: 'ungranted' | 'unsupported' | 'failed'
        readonly detail: string
      }
    }
  | {
      readonly _tag: 'Unavailable'
      /** `ungranted`: the paired device lacks the scope (the read-only dev device); `unsupported`: the gateway lacks the capability. */
      readonly reason: 'ungranted' | 'unsupported' | 'failed'
      readonly detail: string
    }

/** Which adapter feeds the tree: the deterministic fixture world or the st gateway. */
export type DataMode = 'fixtures' | 'live'

/**
 * Row types are wf model types, not fixture types. The world's `WorldHost`/`WorldAgent` must
 * satisfy these (they may carry more); the live adapter maps gateway rows onto them.
 */
export interface Host {
  readonly id: string
  readonly connected: boolean
}

/** One agent session as the fleet projection lists it, keyed by `ref` with its terminal ref beside it. */
export interface Agent {
  readonly ref: string
  readonly terminal: string
  readonly name: string
  readonly host: string
  readonly connected: boolean
  readonly harness?: 'omp' | 'claude' | 'codex'
  readonly activity: 'working' | 'waiting' | 'idle' | 'errored'
  readonly status: string
  readonly state?: string
  readonly description?: string
  /** Last observed activity, not the boundary of the current status. */
  readonly lastActivityAt?: number
  /** Exact observed state boundary when supplied (currently suspension), not last activity. */
  readonly statusSince?: number
  readonly mission?: string
}

/** The fleet projection: every host the gateway knows and the agents running on them. */
export interface Fleet {
  readonly hosts: readonly Host[]
  readonly agents: readonly Agent[]
}

/** The loaded window of one agent conversation; `hasOlder` says whether history continues before `items`. */
export interface ConversationPage {
  readonly items: readonly ConversationItem[]
  readonly hasOlder: boolean
  /** Incremental boundary relative to this exact preceding projection. */
  readonly change?: { readonly from: readonly ConversationItem[]; readonly index: number }
}

/** Independently granted operations; a broad action grant cannot authorize a message send. */
export interface Grants {
  readonly actions: 'granted' | 'ungranted'
  readonly messageSend: 'granted' | 'ungranted'
  readonly terminalInput: 'granted' | 'ungranted'
}

/** One gateway activity line (`at` is ISO-8601) for the events feed. */
export interface GatewayEvent {
  readonly at: string
  readonly source: string
  readonly text: string
}

/**
 * Everything a feature may read: one atom (or atom family) per projection plus the source's
 * clock, grants and usage port. Fixtures and live implement the same shape.
 */
export interface DataSource {
  readonly mode: DataMode
  /** Status bar / live picker bar text, e.g. `fixtures` or `dev gateway · read-only`. */
  readonly label: string
  readonly gateway?: string
  /** Fixtures: the world's fixed base time. Live: `wallClock`. Every relative timestamp reads this. */
  readonly now: Atom.Atom<number>
  readonly grants: Atom.Atom<Grants>
  /** Schema-coupled native family reads; unsupported native producers carry an explicit reason. */
  readonly subjectReads: SubjectReads
  readonly connection: Atom.Atom<ConnectionState>
  readonly agents: Atom.Atom<Feed<readonly AgentRow[]>>
  readonly missions: Atom.Atom<Feed<readonly Mission[]>>
  readonly attention: Atom.Atom<Feed<readonly Attention[]>>
  readonly proposed?: Readonly<Record<string, ProposedMissionFields>>
  readonly events: Atom.Atom<Feed<readonly GatewayEvent[]>>
  /** Keyed by agent ref; retained data follows keep folding while invisible until evicted. */
  readonly conversation: (agentRef: string) => Atom.Atom<Feed<ConversationPage>>
  /** Mounted by visible surfaces in an effect; reading a hidden snapshot never acquires demand. */
  readonly conversationInterest?: (agentRef: string) => Atom.Atom<void>
  /** Start an invisible retained follow on pointer intent or keyboard focus. */
  readonly prefetchConversation?: (agentRef: string) => void
  /** Keyed by terminal ref; live attaches a read-only viewer. */
  readonly terminal: (terminalRef: string) => Atom.Atom<Feed<TerminalScreen>>
  readonly terminalInterest?: (terminalRef: string) => Atom.Atom<void>
  /** Explicit geometry action; never tied to the browser pane dimensions. */
  readonly terminalResize?: TerminalResizePort
  /** Retained owner scrollback, with its own independently granted availability. */
  readonly terminalHistory?: TerminalHistoryFactory
  /** Keyed by subject ref (PR, CI run, mission, pty …). */
  readonly envelope: (ref: string) => Atom.Atom<Feed<SubjectEnvelope>>
  /** Real graph observations, polled while visible until upstream adds resource subscriptions. */
  readonly resources?: AgentResourceSource
  /** The monitor's existing port, unchanged: fixtures declare a synthetic source, live the same-origin relay. */
  readonly usage: MonitorSource
  /** Live client-observed state kept separate from the feature-facing feed projection. */
  readonly sync?: {
    readonly agents: Atom.Atom<FeedSync<readonly AgentRow[]>>
    readonly missions: Atom.Atom<FeedSync<readonly Mission[]>>
    readonly attention: Atom.Atom<FeedSync<readonly Attention[]>>
    readonly conversation: (agentRef: string) => Atom.Atom<FeedSync<ConversationPage>>
    readonly terminal: (terminalRef: string) => Atom.Atom<FeedSync<TerminalScreen>>
  }
}

/** Helpers both sources and `AllStates` stories use. Freshness defaults to `live`. */
export const observed = <A>({
  value,
  freshness = 'live',
}: {
  readonly value: A
  readonly freshness?: 'live' | 'stale'
}): Feed<A> => ({
  _tag: 'Observed',
  value,
  freshness,
})
/** The feed before the first observation arrives. */
export const waiting: Feed<never> = { _tag: 'Waiting' }
/** A feed the source cannot serve; `detail` is the user-facing explanation. */
export const unavailable = ({
  reason,
  detail,
}: {
  readonly reason: 'ungranted' | 'unsupported' | 'failed'
  readonly detail: string
}): Feed<never> => ({
  _tag: 'Unavailable',
  reason,
  detail,
})

/** Live sources' clock for ages and countdowns; ticks once a second while observed. */
export const wallClock: Atom.Atom<number> = Atom.make((get) => {
  const timer = setInterval(() => get.setSelf(Date.now()), 1000)
  get.addFinalizer(() => clearInterval(timer))
  return Date.now()
})
