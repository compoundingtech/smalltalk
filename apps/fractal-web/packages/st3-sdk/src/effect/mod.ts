/**
 * `@st3/sdk/effect`: the st client-v0 transport for wf, as one Effect service.
 *
 * One collections socket carries every follow (`socket.ts`). Each follow is a scoped
 * `Stream<FollowEvent<A>>` decoded tolerantly at this edge with smalltalk's generated codecs, so
 * live drift never breaks the page. Resync and transient failures mark a follow stale; retries
 * coalesce per follow and wait one second before subscribing afresh.
 * Follows hold socket slots under the admission rules in `admission.ts`; an evicted follow's
 * stream ends with a final `Stale`. Every follow also exposes its freshness verdict
 * (`freshness.ts`), folded only from transport events the SDK really observes.
 * Rejected window rows are omitted from decoded items and reported through `onDiagnostics`
 * as `RowDecode` (collection, rowId, revision), once while that rejected revision remains in
 * the window. Reporting state drops departed/recovered rows and superseded revisions.
 * No row contents or decoder errors go to the console.
 *
 * `messageSend` uses the generated HTTP action client and decodes its acknowledgement; it does
 * not retry ambiguous delivery. Daemon refusals retain the complete error envelope and HTTP
 * status, separately from transport failures. `snapshot` reads fresh capabilities over HTTP,
 * bypassing cached discovery so the composer's fence refresh is authoritative.
 * `arrangementActions` exposes the same fresh snapshot/refusal contract as an HTTP-only
 * `submitAction` port, so Sidebar edits do not open a second collections socket.
 *
 * Tracing (`trace.ts`, `firstFrame.ts`): SDK HTTP calls run under their own `st3.*` span (a child
 * of `parentSpan`) whose `traceparent` they carry; socket opens carry the caller's `traceContext`.
 * Each subscribe attempt opens a bounded `wf.ux.first_frame` root (and an `st3.follow.subscribe`
 * child of `parentSpan`) whose context the subscribe command carries.
 * `adoptEarlyCollections` adopts the socket `index.html` opened before the bundle (`early.ts`).
 */
import {
  type Resource as ResourceWire,
  ClientError,
  type ActionOf,
  type CollectionSocket,
  type ErrorEnvelope,
  type CollectionFilters,
  type CollectionFrame,
  type CollectionSocketFactory,
  type CollectionStream,
  isTransientCode,
  St3Client,
} from '@smalltalk/st3-client'
import {
  ActionResult,
  type Capabilities,
  Capabilities as CapabilitiesCodec,
  decodeUnknownSync,
  Resource,
  Runtime,
  TerminalScreen,
  TimelineEntry,
  Snapshot,
} from '@smalltalk/st3-client/schema'
import * as Context from 'effect/Context'
import * as Data from 'effect/Data'
import * as Effect from 'effect/Effect'
import * as Layer from 'effect/Layer'
import * as Option from 'effect/Option'
import * as Deferred from 'effect/Deferred'
import * as Queue from 'effect/Queue'
import * as Stream from 'effect/Stream'
import * as SubscriptionRef from 'effect/SubscriptionRef'
import type * as Tracer from 'effect/Tracer'

import { makeAdmission } from './admission.ts'
import { takeEarlyCollections } from './early.ts'
import { makeFirstFrames, type FirstFrameAttempt, type FirstFrameOutcome } from './firstFrame.ts'
import {
  type FollowFreshness,
  type FreshnessEvent,
  type FreshnessStaleReason,
  initialFreshness,
  transitionFreshness,
} from './freshness.ts'
import { type ConnectionState, type St3Diagnostic, makeChannel } from './socket.ts'
import { makeWindowIngest, type ProcessedWindow } from './windowIngest.ts'
import type { SyncStatus } from './sync-status.ts'
import { syncStatusFromFreshness, syncStatusFromFailure } from './sync-projection.ts'
import { browserSocket, type TraceContext, traceContextOf, traceQuerySocket } from './trace.ts'

export type { SyncStatus, SyncStage, StaleReason, SyncFailureCause } from './sync-status.ts'
export * as SyncStatusSchema from './sync-status-schema.ts'
export { syncStatusFromFreshness, syncStatusFromFailure } from './sync-projection.ts'
export {
  type TraceContext,
  isValidTraceparent,
  traceContextOf,
  traceparentOf,
  validTraceContext,
  wrapTraceFetch,
} from './trace.ts'
export {
  EARLY_COLLECTIONS_GLOBAL,
  type EarlyBootstrap,
  peekEarlyBootstrap,
} from './early.ts'
export {
  FIRST_FRAME_BUDGET_PHASE,
  FIRST_FRAME_SPAN,
  FOLLOW_SUBSCRIBE_SPAN,
  type FirstFrameOutcome,
} from './firstFrame.ts'

// Arrangement windows use the generated owner-scoped subscribeArrangements API.

/**
 * The subscription cap the daemon advertises through its collections capability: `collections`
 * v1 grants 16 concurrent collection subscriptions; older daemons without it hold 8. The probe
 * returns the whole response envelope; the capability list lives in its `value`.
 */
export const advertisedSubscriptionLimit = (envelope: AdvertisedEnvelope): number => {
  const collections = envelope.value.capabilities.find(
    (capability) => capability.id === 'collections',
  )
  return collections !== undefined && collections.state === 'granted' && collections.version >= 1
    ? 16
    : 8
}

/** One capability row of the daemon's advertised list. */
type AdvertisedCapability = {
  readonly id: string
  readonly state: string
  readonly version: number
}

/** The capability list the daemon's connect probe returns, inside its response envelope. */
type AdvertisedEnvelope = {
  readonly value: { readonly capabilities: ReadonlyArray<AdvertisedCapability> }
}
type CollectionName = Parameters<CollectionStream['subscribe']>[1]

export type { ConnectionState, St3Diagnostic } from './socket.ts'
export {
  type FollowFreshness,
  type FreshnessEvent,
  type FreshnessStaleReason,
  initialFreshness,
  transitionFreshness,
} from './freshness.ts'

/** The socket (or the server) has no slot left and every held follow is visible. */
export class SubscriptionLimit extends Data.TaggedError('SubscriptionLimit')<{
  readonly cap: number
}> {}
/** A frame this SDK cannot read at all (not a drifted row: those decode tolerantly). */
export class DecodeFailure extends Data.TaggedError('Decode')<{ readonly message: string }> {}
/** The gateway refused the follow or the credential. */
export class Rejected extends Data.TaggedError('Rejected')<{
  readonly code: string | undefined
  readonly message: string
}> {}
/** The terminal could not be attached (no terminal, runtime gone, attach refused). */
export class AttachFailure extends Data.TaggedError('Attach')<{
  readonly code?: string
  readonly status?: number
  /** Local dependent-read authorization loss, not a fabricated daemon error code. */
  readonly authorizationRefused?: boolean
  readonly message: string
}> {}

/** The daemon refused an action; retain its wire response for caller-specific handling. */
export class ActionRefused extends Data.TaggedError('ActionRefused')<{
  readonly response: ErrorEnvelope
  readonly status: number
  readonly message: string
}> {}
/** No usable action response arrived. Delivery may still have happened. */
export class ActionTransportFailure extends Data.TaggedError('ActionTransportFailure')<{
  readonly cause: unknown
  readonly message: string
}> {}
/** Refusal or uncertain transport failure from an action invocation. */
export type ActionFailure = ActionRefused | ActionTransportFailure

/** Generated message.send input, allowing immutable draft tags. */
export type MessageSendInput = Omit<
  ActionOf<'message.send'>,
  'api_version' | 'type' | 'parameters'
> & {
  readonly parameters: Omit<ActionOf<'message.send'>['parameters'], 'tags'> & {
    readonly tags?: readonly string[]
  }
}

/** Failures reported by a follow instead of terminating its event stream. */
export type FollowFailure = SubscriptionLimit | DecodeFailure | Rejected | AttachFailure

/** A per-follow identity paired with a subscribe command after the socket actually sends it. */
export interface FollowSubscribeSent {
  /** The wire id of this subscribe generation; every (re)subscribe uses a fresh one. */
  readonly id: string
  readonly key: string
}

/**
 * One step of a follow. `Stale`: the socket is down and reconnecting, or the follow was evicted
 * (then it is the stream's last element). The next `Observed` after a `Stale` is authoritative.
 */
export type FollowEvent<A> =
  | { readonly _tag: 'Observed'; readonly value: A }
  | { readonly _tag: 'Stale'; readonly code?: string; readonly message?: string }
  | { readonly _tag: 'Failed'; readonly error: FollowFailure }

/** What a follow follows; `followKey` is its admission identity. */
export type FollowSpec =
  | {
      readonly _tag: 'Window'
      readonly collection: CollectionName
      readonly limit: number
      readonly filters?: CollectionFilters
    }
  | { readonly _tag: 'Conversation'; readonly ref: string }
  | { readonly _tag: 'Terminal'; readonly runtime: string }

/** Stable admission identity shared by a follow and its visibility updates. */
export const followKey = (spec: FollowSpec): string => {
  switch (spec._tag) {
    case 'Window':
      return `window:${spec.collection}:${spec.limit}:${JSON.stringify(spec.filters ?? {})}`
    case 'Conversation':
      return `conversation:${spec.ref}`
    case 'Terminal':
      return `terminal:${spec.runtime}`
  }
}

/** A collection window: decoded rows in the server's order. */
export interface WindowValue {
  readonly items: readonly Resource[]
  readonly hasMore: boolean
  readonly snapshot: ProcessedWindow<Resource>['snapshot']
  readonly rawItems: readonly unknown[]
}

/** A timeline entry the contract codec rejected, kept so it can render as a notice. */
export interface UnrecognizedEntry {
  readonly type: 'unrecognized'
  readonly id: string
  readonly sequence: number
  readonly revision: number
  readonly timestamp?: string
  readonly rawType: string
  readonly raw: unknown
}

/** One conversation frame: `replace` resets the timeline, otherwise `entries` revise it. */
export interface ConversationChunk {
  /** The subscription wire id the gateway addressed this chunk to (one per subscribe generation). */
  readonly id?: string
  readonly replace: boolean
  readonly hasMore: boolean
  readonly entries: readonly (TimelineEntry | UnrecognizedEntry)[]
  /** Raw page evidence before decoding/filtering; only explicit has_more=false proves emptiness. */
  readonly observation?: { readonly empty: boolean }
}

/** Scoped collection follows and gateway state carried by one shared socket. */
export class St3 extends Context.Service<
  St3,
  {
    readonly connection: Stream.Stream<ConnectionState>
    /** Synchronously send a going-away close, then pause automatic reconnect. */
    readonly suspendSockets: () => void
    /** Reconnect and resubscribe retained follows after a bfcache restore; no lifecycle timer. */
    readonly resumeSockets: Effect.Effect<void>
    /** The gateway's capabilities (limits, actions and the scopes this credential holds). */
    readonly capabilities: Effect.Effect<Capabilities, Rejected>
    /** A fresh HTTP snapshot, never the generated client's cached discovery snapshot. */
    readonly snapshot: Effect.Effect<string, ActionFailure>
    readonly messageSend: (request: MessageSendInput) => Effect.Effect<ActionResult, ActionFailure>
    readonly followWindow: (
      spec: Extract<FollowSpec, { _tag: 'Window' }>,
    ) => Stream.Stream<FollowEvent<WindowValue>>
    readonly followConversation: (
      spec: Extract<FollowSpec, { _tag: 'Conversation' }>,
    ) => Stream.Stream<FollowEvent<ConversationChunk>>
    /** Attach a read-only viewer to the runtime's terminal; every (re)subscribe attaches afresh. */
    readonly followTerminal: (
      spec: Extract<FollowSpec, { _tag: 'Terminal' }>,
    ) => Stream.Stream<FollowEvent<TerminalScreen>>
    /** Mount = visible. An invisible follow keeps its slot and keeps folding until evicted. */
    readonly setVisible: (spec: FollowSpec, visible: boolean) => Effect.Effect<void>
    /** Per-follow freshness keyed by followKey; an entry lives while the follow's stream runs. */
    readonly freshness: SubscriptionRef.SubscriptionRef<ReadonlyMap<string, FollowFreshness>>
    /** One follow's freshness verdicts, starting with the current value when one exists. */
    readonly followFreshness: (key: string) => Stream.Stream<FollowFreshness>
    /** Current per-follow statuses; failed retry verdicts remain until a fresh run or SDK release. */
    readonly syncStatuses: SubscriptionRef.SubscriptionRef<ReadonlyMap<string, SyncStatus>>
    readonly followSyncStatus: (key: string) => Stream.Stream<SyncStatus>
    /** Socket reachability is not Live: only a successfully decoded data frame establishes it. */
    readonly gatewaySyncStatus: Stream.Stream<SyncStatus>
  }
>()('@st3/sdk/St3') {}

/** Gateway configuration and transport seams for the scoped SDK layer. */
export interface St3Options {
  /** Gateway origin; the browser passes its own origin (the dev proxy or the serving gateway). */
  readonly baseUrl: string
  /** Socket slots: the daemon's subscription cap (8 on current daemons). */
  readonly maxFollows: number
  /**
   * Conversation follows' dedicated subscription slots, LRU-evicted by view recency with a
   * real unsubscribe; window and terminal follows own the remaining slots, so neither side
   * can starve the other. Undefined shares one pool across every follow; `'advertised'`
   * derives the whole budget from the daemon's advertised collections capability once the
   * connect probe answers (v1 raises the cap to 16), before the first follow can open.
   */
  readonly conversationSlots?: number | 'advertised'
  /** Test seam: the WebSocket the collections socket opens (default: the browser `WebSocket`). */
  readonly socket?: CollectionSocketFactory
  /** Test seam: HTTP reads and actions. */
  readonly fetch?: typeof fetch
  /** Authoritative transitions and frame work, without coupling the SDK to a metrics library. */
  readonly onDiagnostics?: (event: St3Diagnostic) => void
  /** Called with the wire id after a follow's subscribe command is sent on an open WebSocket; includes resubscribe sends. */
  readonly onSubscribeSent?: (id: string) => void
  /** Actual-send callback with the canonical followKey; includes queued-open and resubscribe sends. */
  readonly onFollowSubscribeSent?: (event: FollowSubscribeSent) => void
  /**
   * The caller's active W3C context, read when each HTTP request starts and each socket opens
   * (browser upgrades carry it as URL query parameters the gateway strips).
   */
  readonly traceContext?: () => TraceContext | undefined
  /** The caller's active span; SDK spans (follow subscribes, HTTP reads/actions) become its children. */
  readonly parentSpan?: () => Tracer.Span | undefined
  /**
   * Adopt the collections socket the `index.html` early-connect script opened, for the life of
   * this layer: its roster subscribe answers the SDK's identical one, buffered frames replay.
   */
  readonly adoptEarlyCollections?: boolean
}

const decodeResource = decodeUnknownSync(Resource)

const decodeEntry = decodeUnknownSync(TimelineEntry)
const decodeScreen = decodeUnknownSync(TerminalScreen)
const decodeRuntime = decodeUnknownSync(Runtime)
const decodeCapabilities = decodeUnknownSync(CapabilitiesCodec)
const decodeActionResult = decodeUnknownSync(ActionResult)
const decodeSnapshot = decodeUnknownSync(Snapshot)

const errorMessage = (error: unknown) => (error instanceof Error ? error.message : String(error))
const actionFailure = (cause: unknown): ActionFailure =>
  cause instanceof ClientError
    ? new ActionRefused({
        response: cause.response,
        status: cause.status,
        message: cause.response.message,
      })
    : new ActionTransportFailure({ cause, message: errorMessage(cause) })

/** HTTP-only arrangement actions; sharing this port never opens another collections socket. */
export interface ArrangementActionPort {
  readonly snapshot: Effect.Effect<string, ActionFailure>
  readonly submitAction: (request: ActionOf<'arrangement.edit'>) => Effect.Effect<typeof ActionResult.Type, ActionFailure>
}

export const arrangementActions = (client: Pick<St3Client, 'capabilities' | 'arrangementEdit'>): ArrangementActionPort => ({
  snapshot: Effect.tryPromise({
    try: () => client.capabilities(),
    catch: actionFailure,
  }).pipe(
    Effect.flatMap((envelope) => Effect.try({
      try: () => decodeSnapshot(envelope.snapshot).id,
      catch: actionFailure,
    })),
    Effect.withSpan('st3.snapshot'),
  ),
  submitAction: Effect.fn('st3.arrangement.edit')((request: ActionOf<'arrangement.edit'>) =>
    Effect.tryPromise({
      try: () => client.arrangementEdit(request),
      catch: actionFailure,
    }).pipe(
      Effect.flatMap((envelope) => Effect.try({
        try: () => decodeActionResult(envelope.value),
        catch: actionFailure,
      })),
    )),
})

const unrecognized = (raw: unknown): UnrecognizedEntry | undefined => {
  if (typeof raw !== 'object' || raw === null) return undefined
  const fields = raw as Record<string, unknown>
  if (typeof fields['id'] !== 'string' || typeof fields['sequence'] !== 'number') return undefined
  return {
    type: 'unrecognized',
    id: fields['id'],
    sequence: fields['sequence'],
    revision: typeof fields['revision'] === 'number' ? fields['revision'] : 0,
    ...(typeof fields['timestamp'] === 'string' ? { timestamp: fields['timestamp'] } : {}),
    rawType: typeof fields['type'] === 'string' ? fields['type'] : 'unknown',
    raw,
  }
}

/** Decode entries without confusing discarded payloads or omitted pagination with an empty page. */
export const decodeConversationChunk = (
  frame: Extract<CollectionFrame, { kind: 'conversation' }>,
): ConversationChunk => ({
  id: frame.id,
  replace: frame.replace,
  hasMore: frame.has_more ?? false,
  ...(frame.replace || frame.items.length > 0
    ? { observation: { empty: frame.replace && frame.items.length === 0 && frame.has_more === false } }
    : {}),
  entries: frame.items.flatMap<TimelineEntry | UnrecognizedEntry>((raw) => {
    try {
      return [decodeEntry(raw)]
    } catch {
      return unrecognized(raw) ?? []
    }
  }),
})

/** Legacy uncoded cap errors can reduce admission, but are not a fabricated server code. */
const isSubscriptionLimit = (frame: Extract<CollectionFrame, { kind: 'error' }>) =>
  frame.code === undefined && frame.collection === undefined

const RETRY_DELAY = '1 second'

/** How one kind of follow talks on the socket. */
interface FollowProtocol<A> {
  readonly subscribe: (args: {
    readonly stream: CollectionStream
    readonly id: string
  }) => Effect.Effect<void, FollowFailure>
  /** The decoded value a data frame carries, or `undefined` for a frame that changes nothing. */
  readonly onData: (frame: CollectionFrame) => A | undefined
  /** The socket dropped: forget anything the next snapshot replaces. */
  readonly reset: () => void
}

/** A view of `stream` whose subscribe commands carry `trace`, read synchronously by the client. */
const tracedStream = (
  stream: CollectionStream,
  trace: TraceContext,
  setOutbound: (trace: TraceContext | undefined) => void,
): CollectionStream => {
  const issue =
    <TArgs extends ReadonlyArray<unknown>>(send: (...args: TArgs) => void) =>
    (...args: TArgs) => {
      setOutbound(trace)
      try {
        send(...args)
      } finally {
        setOutbound(undefined)
      }
    }
  return {
    ...stream,
    subscribe: issue(stream.subscribe),
    subscribeGlasses: issue(stream.subscribeGlasses),
    subscribeArrangements: issue(stream.subscribeArrangements),
    subscribeTerminal: issue(stream.subscribeTerminal),
    subscribeConversation: issue(stream.subscribeConversation),
  }
}

const make = (options: St3Options) =>
  Effect.gen(function* () {
    /** A subscribe's own attempt context while the client issues it; otherwise the caller's. */
    let outbound: TraceContext | undefined
    const client = new St3Client({
      baseUrl: options.baseUrl,
      // Remove the browser receiver workaround after smalltalk#1040 / #1247 lands.
      fetchImpl: options.fetch ?? globalThis.fetch.bind(globalThis),
      traceContext: () => outbound ?? options.traceContext?.(),
    })
    const setOutbound = (trace: TraceContext | undefined) => {
      outbound = trace
    }
    const firstFrames = makeFirstFrames({
      tracer: yield* Effect.tracer,
      ...(options.parentSpan === undefined ? {} : { parentSpan: options.parentSpan }),
    })
    /**
     * One SDK HTTP call under its own `st3.*` span, a child of the caller's active span when it has
     * one; the call's requests carry that span's `traceparent`, not the caller's.
     */
    const sdkRequest = <A, E>(name: string, request: (traced: St3Client) => Effect.Effect<A, E>) =>
      Effect.suspend(() => {
        const parent = options.parentSpan?.()
        return Effect.currentSpan.pipe(
          Effect.orDie,
          Effect.flatMap((span) => request(client.withTraceContext(() => traceContextOf(span)))),
          Effect.withSpan(name, parent === undefined ? {} : { parent }),
        )
      })
    // Owned by this layer's scope from the take: released (closed) unless the client adopted it.
    const early = options.adoptEarlyCollections === true
      ? yield* Effect.acquireRelease(
          Effect.sync(() => takeEarlyCollections()),
          (taken) => Effect.sync(() => taken?.close()),
        )
      : undefined
    const freshSocket = traceQuerySocket(options.socket ?? browserSocket)
    let currentSocket: CollectionSocket | undefined
    const socket: CollectionSocketFactory = (url, protocols, headers) => {
      currentSocket = early?.consume() ?? freshSocket(url, protocols, headers)
      return currentSocket
    }
    const followKeys = new Map<string, string>()
    /** Per-follow freshness keyed by followKey while the follow's stream runs. */
    const freshnessTable = new Map<string, FollowFreshness>()
    const freshnessRef = yield* SubscriptionRef.make<ReadonlyMap<string, FollowFreshness>>(
      freshnessTable,
    )
    /** The per-run freshness applier of each live socket subscription id. */
    const fresheners = new Map<string, (event: FreshnessEvent) => void>()
    const publishFreshness = () =>
      Effect.runFork(SubscriptionRef.set(freshnessRef, new Map(freshnessTable)))
    const syncTable = new Map<string, SyncStatus>()
    const syncRef = yield* SubscriptionRef.make<ReadonlyMap<string, SyncStatus>>(new Map())
    const publishSync = () => Effect.runFork(SubscriptionRef.set(syncRef, new Map(syncTable)))
    const syncObservers = new Map<string, number>()
    const admission = makeAdmission<string>({
      cap: options.conversationSlots === 'advertised' ? 8 : options.maxFollows,
      ...(options.conversationSlots === 'advertised'
        ? { conversationSlots: 4 }
        : options.conversationSlots === undefined
          ? {}
          : { conversationSlots: options.conversationSlots }),
    })
    /** Opens once the advertised budget (if derived) is fixed; guards double settlement. */
    const budgetKnown = yield* Deferred.make<void, never>()
    let budgetSettled = false
    const settleBudget = () => {
      if (budgetSettled) return
      budgetSettled = true
      Effect.runFork(Deferred.succeed(budgetKnown, undefined))
    }
    if (options.conversationSlots !== 'advertised') settleBudget()
    /** The open follow per key: its socket id and how to evict it. */
    const open = new Map<string, { readonly id: string; readonly evict: () => void }>()
    const reportFollows = () =>
      options.onDiagnostics?.({
        _tag: 'Follows',
        active: open.size,
        cap: admission.cap(),
        conversationSlots: admission.laneSplit() ? admission.laneCap('conversation') : undefined,
      })
    reportFollows()
    const acceptBudget = (limit: number) => {
      // Initialize once, before admitting follows. Reconnects only shrink this same table:
      // never forget admitted keys/visibility or undo a server-limit refusal. A higher cap
      // is conservatively ignored until a new SDK scope; a lower cap evicts LRU-invisible
      // follows before resubscribe. Visible over-cap follows cannot be evicted and block new
      // opens until they end/become invisible (setVisible below re-runs the trim).
      for (const lane of ['conversation', 'shared'] as const) {
        const target = lane === 'conversation' ? limit - 4 : 4
        const cap = budgetSettled ? Math.min(admission.laneCap(lane), target) : target
        for (const key of admission.resizeLane(lane, cap)) open.get(key)?.evict()
      }
      reportFollows()
      settleBudget()
    }
    const channel = yield* makeChannel({
      client,
      socket,
      parentSpan: options.parentSpan,
      commandTraceContext: () => outbound,
      ...(options.onDiagnostics === undefined ? {} : { onDiagnostics: options.onDiagnostics }),
      ...(options.conversationSlots === 'advertised'
        ? {
            onCapabilities: (envelope: AdvertisedEnvelope) => {
              acceptBudget(advertisedSubscriptionLimit(envelope))
            },
            onProbeFailure: () => {
              // A refused or unavailable first probe still releases every admission waiter.
              // Permanent rejection is reported by the channel to each follow, not hidden
              // behind this conservative 8/4 fallback.
              if (!budgetSettled) acceptBudget(8)
            },
          }
        : {}),
      onSubscribeSent: ({ id, wire }) => {
        options.onSubscribeSent?.(wire)
        fresheners.get(id)?.({ _tag: 'SubscribeSent' })
        const key = followKeys.get(id)
        if (key !== undefined) options.onFollowSubscribeSent?.({ id: wire, key })
      },
    })
    // The channel's Reconnecting states carry the attempt and issue each per-follow drop lacks.
    yield* Effect.forkIn(
      SubscriptionRef.changes(channel.connection).pipe(
        Stream.runForEach((state) =>
          Effect.sync(() => {
            if (state._tag !== 'Reconnecting') return
            for (const apply of fresheners.values())
              apply({ _tag: 'SocketDropped', attempt: state.attempt, issue: state.issue, nextAt: state.nextAt })
          }),
        ),
      ),
      yield* Effect.scope,
    )
    let nextId = 0

    const evict = (key: string | undefined) => {
      if (key !== undefined) open.get(key)?.evict()
    }

    /** `makeProtocol` runs once per stream run, so a re-run follow starts from fresh state. */
    const follow = <A>({
      spec,
      makeProtocol,
    }: {
      readonly spec: FollowSpec
      readonly makeProtocol: (observe: (value: A, snapshot?: unknown) => void) => FollowProtocol<A>
    }) =>
      Stream.callback<FollowEvent<A>>((queue) =>
        Effect.gen(function* () {
          const key = followKey(spec)
          const id = `f${(nextId += 1)}`
          followKeys.set(id, key)
          let freshness = initialFreshness()
          let lastLiveAt: number | undefined
          let snapshot: unknown
          let ownedSync: SyncStatus
          const setSync = (status: SyncStatus) => {
            ownedSync = status
            syncTable.set(key, status)
            publishSync()
          }
          setSync({ _tag: 'Connecting', attempt: 1, since: Date.now() })
          const applyFreshness = (event: FreshnessEvent) => {
            const next = transitionFreshness(freshness, event, Date.now())
            if (next === freshness) {
              // Window evidence advances even while the transport stays Live.
              if (event._tag === 'Frame' && next._tag === 'Live' && snapshot !== undefined)
                setSync({ _tag: 'Live', since: next.since, snapshot })
              return
            }
            freshness = next
            freshnessTable.set(key, next)
            publishFreshness()
            if (next._tag === 'Live') lastLiveAt = next.since
            const status = syncStatusFromFreshness(next, lastLiveAt)
            setSync(status._tag === 'Live' && snapshot !== undefined ? { ...status, snapshot } : status)
          }
          fresheners.set(id, applyFreshness)
          const emit = (event: FollowEvent<A>) => {
            Queue.offerUnsafe(queue, event)
          }
          /** The current subscribe attempt's first-frame spans, until its first decoded frame. */
          let attempt: FirstFrameAttempt | undefined
          const endAttempt = (outcome: FirstFrameOutcome) => {
            attempt?.end(outcome)
            attempt = undefined
          }
          const observe = (value: A, decodedSnapshot?: unknown) => {
            endAttempt('observed')
            if (decodedSnapshot !== undefined) snapshot = decodedSnapshot
            lastLiveAt = Date.now()
            applyFreshness({ _tag: 'Frame' })
            channel.onDecodedFrame()
            emit({ _tag: 'Observed', value })
          }
          const protocol = makeProtocol(observe)
          // Status writes precede the terminal FollowEvent so no consumer teardown races a verdict.
          const fail = (error: FollowFailure, status = syncStatusFromFailure(error)) => {
            setSync(status)
            endAttempt('failed')
            emit({ _tag: 'Failed', error })
            stop()
          }
          const stop = () => {
            endAttempt('disposed')
            followKeys.delete(id)
            fresheners.delete(id)
            if (freshnessTable.get(key) === freshness) {
              freshnessTable.delete(key)
              publishFreshness()
            }
            protocol.reset()
            channel.unregister(id)
            if (open.get(key)?.id === id) {
              open.delete(key)
              admission.close(key)
              reportFollows()
            }
            Queue.endUnsafe(queue)
          }
          // Register cleanup before the interruptible budget latch: cancellation while the
          // first probe is pending must remove routing/freshness entries too.
          yield* Effect.addFinalizer(() => Effect.sync(stop))
          yield* Effect.addFinalizer(() => Effect.sync(() => {
            // A superseding run with the same key owns its own current status.
            // Failed is retained as the retry verdict, not a cache of source content.
            if (syncTable.get(key) === ownedSync && ownedSync._tag !== 'Failed') {
              syncTable.delete(key)
              publishSync()
            }
          }))
          const lane = spec._tag === 'Conversation' ? 'conversation' : 'shared'
          // Under `'advertised'`, no follow admits before the connect probe fixes the budget:
          // a subscribe cannot send before that probe opens the socket anyway.
          if (options.conversationSlots === 'advertised') yield* Deferred.await(budgetKnown)
          const admitted = admission.open(key, lane)
          if (admitted._tag === 'Full') {
            fail(new SubscriptionLimit({ cap: admission.cap() }))
            return
          }
          evict(admitted.evict)
          open.set(key, {
            id,
            evict: () => {
              endAttempt('disposed')
              applyFreshness({ _tag: 'Evicted' })
              emit({ _tag: 'Stale' })
              protocol.reset()
              channel.unregister(id)
              open.delete(key)
              reportFollows()
              Queue.endUnsafe(queue)
            },
          })
          reportFollows()

          const scope = yield* Effect.scope
          const context = yield* Effect.context()
          const subscribe = ({ stream, id: wireId }: { readonly stream: CollectionStream; readonly id: string }) =>
            Effect.suspend(() => {
              endAttempt('superseded')
              // The early-connect roster subscribe answers an identical first window subscribe.
              const adopted = spec._tag === 'Window'
                ? early?.wouldAdopt({ kind: 'subscribe', collection: spec.collection, limit: spec.limit, ...spec.filters })
                : undefined
              const current = firstFrames.begin({
                kind: spec._tag === 'Window' ? 'window' : spec._tag === 'Conversation' ? 'conversation' : 'terminal',
                label: spec._tag === 'Window' ? spec.collection : spec._tag.toLowerCase(),
                ...(adopted === undefined ? {} : { adopted }),
              })
              attempt = current
              return protocol.subscribe({ stream: tracedStream(stream, current.trace, setOutbound), id: wireId })
            }).pipe(Effect.catch((error) => Effect.sync(() => fail(error))))
          let retryPending = false
          const retryLater = Effect.suspend(() => {
            if (retryPending) return Effect.void
            retryPending = true
            return Effect.asVoid(
              Effect.forkIn(
                Effect.andThen(Effect.sleep(RETRY_DELAY), channel.resubscribe(id)).pipe(
                  Effect.ensuring(Effect.sync(() => (retryPending = false))),
                ),
                scope,
              ),
            )
          })
          yield* channel.register({
            id,
            subscriber: {
              subscribe,
              onRejected: (message, code) => fail(new Rejected({ code, message })),
              onDrop: () => {
                endAttempt('dropped')
                protocol.reset()
                applyFreshness({
                  _tag: 'SocketDropped',
                  attempt: 1,
                  issue: 'the collections socket ended',
                })
                emit({ _tag: 'Stale' })
              },
              onFrame: (frame) => {
                if (frame.kind === 'resync') {
                  options.onDiagnostics?.({ _tag: 'Resync' })
                  protocol.reset()
                  applyFreshness({
                    _tag: 'Resync',
                    ...(frame.code === undefined ? {} : { code: frame.code }),
                    ...(frame.message === undefined ? {} : { message: frame.message }),
                  })
                  emit({
                    _tag: 'Stale',
                    ...(frame.code === undefined ? {} : { code: frame.code }),
                    ...(frame.message === undefined ? {} : { message: frame.message }),
                  })
                  Effect.runForkWith(context)(retryLater)
                  return
                }
                if (frame.kind === 'error') {
                  if (frame.code === 'subscription-limit') {
                    fail(new Rejected({ code: frame.code, message: frame.message }))
                    return
                  }
                  if (isSubscriptionLimit(frame)) {
                    applyFreshness({ _tag: 'Retry' })
                    const retried = admission.serverLimit(key, lane)
                    reportFollows()
                    if (retried._tag === 'Full') {
                      fail(
                        new SubscriptionLimit({ cap: admission.cap() }),
                        { _tag: 'Failed', cause: { _tag: 'Unknown' } },
                      )
                      return
                    }
                    evict(retried.evict)
                    Effect.runForkWith(context)(channel.resubscribe(id))
                    return
                  }
                  if (isTransientCode(frame.code)) {
                    protocol.reset()
                    applyFreshness({
                      _tag: 'Retry',
                      ...(frame.code === undefined ? {} : { code: frame.code }),
                      ...(frame.message === undefined ? {} : { message: frame.message }),
                    })
                    emit({
                      _tag: 'Stale',
                      ...(frame.code === undefined ? {} : { code: frame.code }),
                      ...(frame.message === undefined ? {} : { message: frame.message }),
                    })
                    Effect.runForkWith(context)(retryLater)
                    return
                  }
                  fail(
                    spec._tag === 'Terminal'
                      ? new AttachFailure({ message: frame.message, ...(frame.code === undefined ? {} : { code: frame.code }) })
                      : new Rejected({ code: frame.code, message: frame.message }),
                  )
                  return
                }
                const started =
                  options.onDiagnostics === undefined || spec._tag === 'Window'
                    ? undefined
                    : performance.now()
                let value: A | undefined
                try {
                  value = protocol.onData(frame)
                } finally {
                  if (started !== undefined)
                    options.onDiagnostics?.({
                      _tag: 'Decode',
                      elapsedMs: performance.now() - started,
                    })
                }
                if (value !== undefined) observe(value)
              },
            },
          })
        }),
      )

    const followWindow = (spec: Extract<FollowSpec, { _tag: 'Window' }>) =>
      follow<WindowValue>({
        spec,
        makeProtocol: (observe) => {
          const ingest = makeWindowIngest<Resource>({
            publish: (window) => observe(window, window.snapshot),
            ...(options.onDiagnostics === undefined
              ? {}
              : {
                  onSlice: (elapsedMs: number) =>
                    options.onDiagnostics?.({ _tag: 'Decode', elapsedMs }),
                  onRejected: (row: ResourceWire) =>
                    options.onDiagnostics?.({
                      _tag: 'RowDecode', collection: spec.collection,
                      rowId: row.id, revision: row.revision,
                    }),
                }),
            decode: (row) => {
              try {
                return decodeResource(row)
              } catch {
                return undefined
              }
            },
          })
          return {
            subscribe: ({ stream, id }) =>
              Effect.sync(() => stream.subscribe(id, spec.collection, spec.limit, spec.filters)),
            onData: (frame) => {
              ingest.accept(frame)
              return undefined
            },
            reset: ingest.reset,
          }
        },
      })

    const followConversation = (spec: Extract<FollowSpec, { _tag: 'Conversation' }>) =>
      follow<ConversationChunk>({
        spec,
        makeProtocol: () => ({
          subscribe: ({ stream, id }) =>
            Effect.sync(() => stream.subscribeConversation(id, spec.ref)),
          onData: (frame) => {
            if (frame.kind !== 'conversation') return undefined
            return decodeConversationChunk(frame)
          },
          reset: () => {},
        }),
      })

    /** `runtimesGet` → attach fenced on the runtime incarnation → subscribe with the capability. */
    const attachTerminal = (runtimeRef: string) =>
      Effect.gen(function* () {
        const read = yield* Effect.tryPromise({
          try: () => client.runtimesGet(runtimeRef),
          catch: (error) => error instanceof ClientError
            ? new AttachFailure({ code: error.response.code, status: error.status, message: error.response.message })
            : new AttachFailure({ message: errorMessage(error) }),
        })
        const runtime = yield* Effect.try({
          try: () => decodeRuntime(read.value),
          catch: (error) => new DecodeFailure({ message: errorMessage(error) }),
        })
        if (Option.isNone(runtime.terminal_id) || Option.isNone(runtime.incarnation_id)) {
          return yield* new AttachFailure({ message: `${runtimeRef} has no live terminal` })
        }
        const terminal = runtime.terminal_id.value
        const incarnation = runtime.incarnation_id.value
        const result = yield* Effect.tryPromise({
          try: () =>
            client.terminalAttach({
              id: `action/wf-${crypto.randomUUID()}`,
              idempotency_key: crypto.randomUUID(),
              fence: {
                snapshot_id: read.snapshot.id,
                subject_revisions: {},
                runtime_incarnation: incarnation,
              },
              parameters: { target_id: terminal },
            }),
          catch: (error) => error instanceof ClientError
            ? new AttachFailure({ code: error.response.code, status: error.status, message: error.response.message })
            : new AttachFailure({ message: errorMessage(error) }),
        })
        const attachment = result.value.terminal_attachment
        if (attachment === undefined || attachment === null || !attachment.stream_capability) {
          return yield* new AttachFailure({ message: 'the gateway returned no terminal stream' })
        }
        return {
          terminal,
          incarnation: attachment.runtime_incarnation,
          capability: attachment.stream_capability,
        }
      })

    const followTerminal = (spec: Extract<FollowSpec, { _tag: 'Terminal' }>) =>
      follow<TerminalScreen>({
        spec,
        makeProtocol: () => ({
          subscribe: ({ stream, id }) =>
            Effect.map(attachTerminal(spec.runtime), ({ terminal, incarnation, capability }) =>
              stream.subscribeTerminal(id, terminal, incarnation, capability),
            ),
          onData: (frame) => {
            if (frame.kind !== 'screen') return undefined
            try {
              return decodeScreen(frame.value)
            } catch (error) {
              console.warn('st3 terminal screen did not decode', error)
              return undefined
            }
          },
          reset: () => {},
        }),
      })

    return St3.of({
      connection: SubscriptionRef.changes(channel.connection),
      suspendSockets: () => {
        // Send synchronously before Effect interruption/finalization or document teardown.
        currentSocket?.close(1001)
        early?.close(1001)
        channel.suspend()
      },
      resumeSockets: channel.resume,
      freshness: freshnessRef,
      followFreshness: (key) =>
        SubscriptionRef.changes(freshnessRef).pipe(
          Stream.map((table) => table.get(key)),
          Stream.filter((value): value is FollowFreshness => value !== undefined),
        ),
      syncStatuses: syncRef,
      followSyncStatus: (key) =>
        SubscriptionRef.changes(syncRef).pipe(
          Stream.map((table) => table.get(key)),
          Stream.filter((value): value is SyncStatus => value !== undefined),
          Stream.changes,
          Stream.onStart(Effect.sync(() => {
            syncObservers.set(key, (syncObservers.get(key) ?? 0) + 1)
          })),
          Stream.ensuring(Effect.sync(() => {
            const remaining = (syncObservers.get(key) ?? 1) - 1
            if (remaining > 0) {
              syncObservers.set(key, remaining)
              return
            }
            syncObservers.delete(key)
            if (syncTable.get(key)?._tag === 'Failed') {
              syncTable.delete(key)
              publishSync()
            }
          })),
        ),
      gatewaySyncStatus: SubscriptionRef.changes(channel.syncStatus).pipe(Stream.changes),
      capabilities: sdkRequest('st3.capabilities', (traced) =>
        Effect.tryPromise({
          try: () => traced.discover(),
          catch: (error) => new Rejected({
            code: error instanceof ClientError ? error.response.code : undefined,
            message: error instanceof ClientError ? error.response.message : errorMessage(error),
          }),
        }).pipe(
          Effect.flatMap((envelope) =>
            Effect.try({
              try: () => decodeCapabilities(envelope.value),
              catch: (error) => new Rejected({ code: undefined, message: errorMessage(error) }),
            }),
          ),
        ),
      ),
      snapshot: sdkRequest('st3.snapshot', (traced) =>
        Effect.tryPromise({
          try: () => traced.capabilities(),
          catch: actionFailure,
        }).pipe(
          Effect.flatMap((envelope) =>
            Effect.try({
              try: () => decodeSnapshot(envelope.snapshot).id,
              catch: actionFailure,
            }),
          ),
        ),
      ),
      messageSend: (request: MessageSendInput) =>
        sdkRequest('st3.messageSend', (traced) =>
          Effect.tryPromise({
            try: () => {
              const { tags, ...parameters } = request.parameters
              return traced.messageSend({
                id: request.id,
                idempotency_key: request.idempotency_key,
                fence: request.fence,
                parameters: {
                  ...parameters,
                  ...(tags === undefined ? {} : { tags: [...tags] }),
                },
              })
            },
            catch: actionFailure,
          }).pipe(
            Effect.flatMap((envelope) =>
              Effect.try({
                try: () => decodeActionResult(envelope.value),
                catch: actionFailure,
              }),
            ),
          ),
        ),
      followWindow,
      followConversation,
      followTerminal,
      setVisible: (spec, visible) =>
        Effect.sync(() => {
          const lane = spec._tag === 'Conversation' ? 'conversation' : 'shared'
          admission.setVisible({ key: followKey(spec), visible })
          for (const key of admission.resizeLane(lane, admission.laneCap(lane)))
            open.get(key)?.evict()
        }),
    })
  })

/** The SDK for the life of the layer: one socket, opened on build, closed on release. */
export const St3Live = (options: St3Options) => Layer.effect(St3, make(options))
