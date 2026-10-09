import { ClientError, St3Client } from '@smalltalk/st3-client'
import { Runtime, Snapshot, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import type { Agent, Attention, Mission, TerminalScreen } from '@smalltalk/st3-client/schema'
import {
  St3,
  St3Live,
  AttachFailure,
  type FollowEvent,
  type FollowSpec,
  type ConnectionState,
  followKey,
  type St3Options,
  type SyncStatus,
  syncStatusFromFailure,
} from '@st3/sdk/effect'
import { Effect, Fiber, Layer, ManagedRuntime, Metric, Option, Schema, Stream, SubscriptionRef } from 'effect'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'

/** One SDK runtime and one frame writer for the application's retained live projections. */
import { incrDebug, setDebug } from '../telemetry/meters.tsx'

import { LiveTimeline } from '../conversation/fromTimeline.ts'
import type { TextItem } from '../conversation/model.ts'
import { undeclared } from '../monitor/source.ts'
import { gatewayResources } from '../resources/agent/source.ts'
import { instrumentFetch } from '../telemetry/transport.ts'
import type { UxTelemetry } from '../telemetry/ux.ts'
import { getDebug } from '../telemetry/measurement/index.ts'
import { unavailableTerminalHistory } from '../terminal/historySource.ts'
import { gatewayTerminalResize } from '../terminal/terminal-resize-port.ts'
import { gatewayAttachments, gatewayMessageSend } from './attachmentPort.ts'
import { gatewayContentSearch } from './contentSearchPort.ts'
import { makeFrameIngest } from './frameIngest.ts'
import { nativeAgentFetch } from './nativeAgentFetch.ts'
import { mergeOutboxItems } from './outbox.ts'
import {
  initialFeedSync,
  observeFeedSync,
  transitionFeedSync,
  type FeedSync,
} from './feedSync.ts'
import {
  type ConversationPage,
  type AttachmentPort,
  type ConversationPortResult,
  type DataSource,
  type Feed,
  type Grants,
  observed,
  unavailable,
  waiting,
  wallClock,
} from './source.ts'
import { gatewaySubjectReads, SubjectReadPort, subjectReaderFromAtom } from './subjectReadPort.ts'
import { gatewaySessionTraceLayer } from './stSessionTrace.ts'
import { sessionTrace, type SessionTraceProvider } from './sessionTrace.ts'

/** The owned source registry and runtime teardown handle. */
export interface LiveSource {
  readonly source: DataSource
  readonly registry: AtomRegistry.AtomRegistry
  /** Selection owns the real data-ready read, not a claim that a transcript is painted. */
  readonly selectConversation: (ref: string) => void
  /** Initialize the shared tracer before the first DOM commit. */
  readonly ready: Promise<void>
  readonly suspendSockets: () => void
  readonly resumeSockets: () => void
  readonly dispose: () => Promise<void>
}

interface RetainedFeed<A> {
  readonly atom: Atom.Atom<Feed<A>>
  readonly interest: Atom.Atom<void>
  readonly snapshot: Atom.Atom<Feed<A>>
  readonly controller: Atom.Atom<Feed<A>>
  readonly sync: Atom.Atom<FeedSync<A>>
  /** Whether the value became visible; `false` means the feed is not publishable. */
  readonly publish: (value: A) => boolean
  readonly prefetch: () => void
  /** Re-acquire a failed or ended follow; a healthy follow is untouched. */
  readonly retry: () => void
  readonly release: () => void
}

interface RetainedConversation extends RetainedFeed<ConversationPage> {
  readonly send: AttachmentPort['send']
}

interface PendingSend {
  item: TextItem
}

/** Connect retained workbench projections through one SDK runtime and frame writer. */
export const liveSource = ({
  options,
  telemetryLayer,
  ux,
  sessionTraceLayer,
}: {
  readonly options: St3Options
  /** Browser root owns sampling and observers; the SDK shares its scoped tracer/exporter. */
  readonly telemetryLayer?: Layer.Layer<never> | undefined
  readonly ux?: () => UxTelemetry
  /** Optional independent trace provider; the public default only knows native st meters. */
  readonly sessionTraceLayer?: Layer.Layer<SessionTraceProvider> | undefined
}): LiveSource => {
  const { baseUrl } = options
  const origin = typeof location === 'undefined' ? new URL(baseUrl).origin : location.origin
  const fetch = instrumentFetch({
    fetchImpl: nativeAgentFetch({ origin, fetchImpl: options.fetch ?? globalThis.fetch.bind(globalThis) }),
    origin,
    traceContext: options.traceContext,
  })
  const deniedReaders = new Set<(message: string) => void>()
  let readRejection: string | undefined
  let connectionAttempt = 1
  const noGrants: Grants = {
    actions: 'ungranted',
    messageSend: 'ungranted',
    terminalInput: 'ungranted',
  }
  setDebug('Wf.socketErrors', 0)
  setDebug('Wf.socketLive', 0)
  setDebug('Wf.activeFollows', 0)
  setDebug('Wf.followCap', options.maxFollows)
  setDebug('Wf.conversationEntries', 0)
  if (typeof options.conversationSlots === 'number')
    setDebug('Wf.conversationSlots', options.conversationSlots)
  // Each sync-status consumer is owned by the corresponding follow fiber.
  let freshnessConsumers = 0
  setDebug('Wf.freshnessConsumers', 0)
  const client = new St3Client({
    baseUrl,
    // Remove the browser receiver workaround after smalltalk#1040 / #1247 lands.
    fetchImpl: fetch,
  })
  const registry = AtomRegistry.make()
  const readRefusal = Atom.keepAlive(Atom.make<string | undefined>(undefined))
  const resources = gatewayResources({
    baseUrl,
    fetchImpl: fetch,
  })
  const subjectReads = gatewaySubjectReads({
    options: { baseUrl, fetchImpl: fetch },
    resource: subjectReaderFromAtom({ feed: resources.byId, registry }),
    discovery: client,
    registry,
  })
  const runtime = ManagedRuntime.make(
    St3Live({
      ...options,
      fetch,
      onDiagnostics: (event) => {
        switch (event._tag) {
          case 'Connection':
            if (event.state._tag === 'Reconnecting') connectionAttempt = event.state.attempt
            setDebug('Wf.socketLive', event.state._tag === 'Live' ? 1 : 0)
            if (event.state._tag === 'Rejected') {
              readRejection = event.state.message
              registry.set(readRefusal, event.state.message)
              for (const deny of deniedReaders) deny(event.state.message)
            }
            if (event.state._tag === 'Rejected' || event.state._tag === 'Reconnecting')
              incrDebug('Wf.socketErrors')
            break
          case 'Follows':
            setDebug('Wf.activeFollows', event.active)
            setDebug('Wf.followCap', event.cap)
            if (event.conversationSlots !== undefined)
              setDebug('Wf.conversationSlots', event.conversationSlots)
            break
          case 'Frame':
            incrDebug('Wf.frames')
            break
          case 'Resync':
            incrDebug('Wf.resyncs')
            break
          case 'Retry':
            incrDebug('Wf.retries')
            break
          case 'Decode':
            setDebug('Wf.decodeMs', event.elapsedMs)
            break
        }
        options.onDiagnostics?.(event)
      },
    }).pipe(
      Layer.provideMerge(Metric.enableRuntimeMetricsLayer),
      Layer.merge(Layer.succeed(SubjectReadPort, subjectReads)),
      Layer.merge(sessionTraceLayer ?? gatewaySessionTraceLayer(client).pipe(
        Layer.provide(Layer.succeed(SubjectReadPort, subjectReads)),
      )),
      Layer.provideMerge(telemetryLayer ?? Layer.empty),
    ),
  )
  let sdk: St3['Service'] | undefined
  let disposed = false
  const ingest = makeFrameIngest<() => void, () => void>({ write: ({ value }) => value() })

  const retain = <A, TSpec extends FollowSpec>({
    resolve,
    follow,
    keepAlive = true,
    onCommit,
    explicitInterest = false,
    onVisibilityChange,
    telemetryKind = 'window',
  }: {
    readonly resolve: (get: Atom.AtomContext) => Effect.Effect<TSpec, AttachFailure>
    readonly follow: (args: {
      readonly st3: St3['Service']
      readonly spec: TSpec
    }) => Stream.Stream<FollowEvent<A>>
    readonly keepAlive?: boolean
    readonly onCommit?: (value: A) => void
    readonly explicitInterest?: boolean
    readonly onVisibilityChange?: (visible: boolean) => void
    readonly telemetryKind?: 'window' | 'conversation' | 'terminal'
  }) => {
    let visible = false
    let ended = false
    let terminalFailure = false
    let spec: TSpec | undefined
    let previousFiber: Fiber.Fiber<void> | undefined
    let latest: Feed<A> =
      readRejection === undefined
        ? waiting
        : unavailable({ reason: 'ungranted', detail: readRejection })
    let syncLatest = initialFeedSync<A>(Date.now(), connectionAttempt)
    if (readRejection !== undefined)
      syncLatest = transitionFeedSync(
        syncLatest,
        {
          _tag: 'Failed',
          cause: {
            _tag: 'Local',
            kind: 'connection-rejected',
            detail: { message: readRejection },
          },
        },
        Date.now(),
      )
    const syncData = Atom.keepAlive(Atom.make<FeedSync<A>>(syncLatest))
    let unmount: (() => void) | undefined
    const data = Atom.make<Feed<A>>(latest)
    const following = Atom.make((get): Feed<A> => {
      let active = true
      if (readRejection !== undefined)
        latest = unavailable({ reason: 'ungranted', detail: readRejection })
      else if (latest._tag === 'Observed') {
        latest = { ...latest, freshness: 'stale' }
        syncLatest = transitionFeedSync(syncLatest, { _tag: 'Stale', reason: { _tag: 'Unknown' } }, Date.now())
      }
      registry.set(data, latest)
      registry.set(syncData, syncLatest)
      const commit = () => {
        if (active) {
          if (latest._tag === 'Observed') onCommit?.(latest.value)
          ux?.().observeSync({ key: syncData, kind: telemetryKind, status: syncLatest.sync.status })
          registry.set(data, latest)
          registry.set(syncData, syncLatest)
        }
      }
      const setSync = (status: SyncStatus) => {
        if (terminalFailure && status._tag !== 'Failed') return
        ux?.().observeSync({ key: syncData, kind: telemetryKind, status })
        syncLatest = transitionFeedSync(syncLatest, status, Date.now())
        if (status._tag !== 'Live' && latest._tag === 'Observed')
          latest = { ...latest, freshness: 'stale' }
        ingest.accept({ key: commit, value: commit })
      }
      const deny = (message: string) => {
        setSync({
          _tag: 'Failed',
          cause: { _tag: 'Local', kind: 'connection-rejected', detail: { message } },
        })
        latest = unavailable({ reason: 'ungranted', detail: message })
        ingest.accept({ key: commit, value: commit })
      }
      deniedReaders.add(deny)
      ended = false
      terminalFailure = false
      // Read reactive ownership before forking; dependencies must belong to this atom run.
      const resolving = resolve(get)
      const previous = previousFiber
      spec = undefined
      let freshnessFiber: Fiber.Fiber<void, unknown> | undefined
      const fiber = runtime.runFork(
        Effect.gen(function* () {
          // Release the old socket slot before binding the replacement owner.
          if (previous !== undefined) yield* Fiber.interrupt(previous)
          if (readRejection !== undefined) {
            terminalFailure = true
            setSync({
              _tag: 'Failed',
              cause: {
                _tag: 'Local',
                kind: 'connection-rejected',
                detail: { message: readRejection },
              },
            })
            return
          }
          const st3 = yield* St3
          const boundSpec = yield* resolving
          spec = boundSpec
          const key = followKey(boundSpec)
          freshnessFiber = runtime.runFork(
            Effect.gen(function* () {
              freshnessConsumers += 1
              setDebug('Wf.freshnessConsumers', freshnessConsumers)
              yield* st3.followSyncStatus(key).pipe(
                Stream.runForEach((status) =>
                  Effect.sync(() => {
                    if (!active || readRejection !== undefined) return
                    setSync(status)
                  }),
                ),
              )
            }).pipe(
              Effect.ensuring(
                Effect.sync(() => {
                  freshnessConsumers -= 1
                  setDebug('Wf.freshnessConsumers', freshnessConsumers)
                }),
              ),
            ),
          )
          yield* st3.setVisible(boundSpec, visible)
          yield* follow({ st3, spec: boundSpec }).pipe(
            Stream.runForEach((event) =>
              Effect.sync(() => {
                if (!active || readRejection !== undefined) return
                if (event._tag === 'Observed') {
                  latest = observed({ value: event.value })
                  syncLatest = observeFeedSync(syncLatest, event.value, Date.now())
                } else if (event._tag === 'Failed') {
                  const failure = syncStatusFromFailure(event.error)
                  syncLatest = transitionFeedSync(syncLatest, failure, Date.now())
                  terminalFailure = true
                  // Keep trusted content for failed reads, never authorization-revoked rows.
                  const authorizationRefusal =
                    event.error._tag === 'Attach'
                      ? event.error.code === 'forbidden' || event.error.status === 401 || event.error.status === 403
                      : event.error._tag === 'Rejected' && event.error.code === 'forbidden'
                  latest =
                    latest._tag === 'Observed' && !authorizationRefusal
                      ? {
                          ...latest,
                          freshness: 'stale',
                          error: {
                            reason: 'failed',
                            detail: event.error.message,
                          },
                        }
                      : unavailable({
                          reason: authorizationRefusal ? 'ungranted' : 'failed',
                          detail: event.error.message,
                          code: failure._tag === 'Failed' && failure.cause._tag === 'Server' ? failure.cause.code : undefined,
                        })
                } else {
                  // SDK sync status owns the verdict; this event only degrades retained content.
                  if (latest._tag === 'Observed') latest = { ...latest, freshness: 'stale' }
                }
                ingest.accept({ key: commit, value: commit })
              }),
            ),
          )
        }).pipe(
          Effect.catch((error) =>
            Effect.sync(() => {
              if (!active || readRejection !== undefined) return

              terminalFailure = true
              const failure = syncStatusFromFailure(error)
              syncLatest = transitionFeedSync(syncLatest, failure, Date.now())
              const authorizationRefusal =
                error.authorizationRefused === true || error.code === 'forbidden' || error.status === 401 || error.status === 403
              latest = latest._tag === 'Observed' && !authorizationRefusal
                ? { ...latest, freshness: 'stale', error: { reason: 'failed', detail: error.message } }
                : unavailable({
                  reason: authorizationRefusal ? 'ungranted' : 'failed', detail: error.message,
                  code: failure._tag === 'Failed' && failure.cause._tag === 'Server' ? failure.cause.code : undefined,
                })
              ingest.accept({ key: commit, value: commit })
            }),
          ),
          Effect.ensuring(
            Effect.suspend(() =>
              (freshnessFiber === undefined ? Effect.void : Fiber.interrupt(freshnessFiber)).pipe(
                Effect.andThen(
                  Effect.sync(() => {
                    if (!active) return
                    ended = true
                    // Interest can return between SDK eviction and this finalizer. Its
                    // acquisition saw ended=false, so completion must hand demand back
                    // to a new run too. Wait until this fiber finishes before refreshing.
                    if (visible && !terminalFailure && readRejection === undefined) queueMicrotask(() => {
                      if (!active || !visible || !ended || terminalFailure || readRejection !== undefined) return
                      ingest.flush()
                      registry.refresh(following)
                    })
                  }),
                ),
              ),
            ),
          ),
        ),
      )
      previousFiber = fiber
      get.addFinalizer(() => {
        active = false
        deniedReaders.delete(deny)
        fiber.interruptUnsafe()
      })
      return latest
    }).pipe(keepAlive ? Atom.keepAlive : Atom.autoDispose)
    // Atom.family holds its value weakly. Either exported atom must retain this
    // controller wrapper while mounted, not just its underlying decoded value.
    let retained: RetainedFeed<A>
    const interest = Atom.make((get) => {
      visible = true
      onVisibilityChange?.(true)
      if (explicitInterest) unmount ??= registry.mount(following)
      // An ended follow holds no socket subscription: re-acquired interest re-opens it as
      // a fresh switch, whatever ended it. A Failed run re-runs and re-fails on its own
      // verdict (an authorization refusal stays Unavailable), so this cannot loop.
      if (ended && readRejection === undefined) {
        ingest.flush()
        get.refresh(following)
      }
      const current = spec
      if (current !== undefined)
        runtime.runFork(Effect.flatMap(St3, (st3) => st3.setVisible(current, true)))
      get.addFinalizer(() => {
        visible = false
        onVisibilityChange?.(false)
        const hidden = spec
        if (hidden !== undefined)
          runtime.runFork(Effect.flatMap(St3, (st3) => st3.setVisible(hidden, false)))
      })
      get.mount(retained.controller)
    })
    const atom = Atom.make((get) => {
      if (!explicitInterest) get(interest)
      // Establish frame-coalesced notification ownership, but a newly selected reader
      // can use the already decoded snapshot without waiting for the next writer frame.
      get(retained.snapshot)
      // Snapshot-only readers have no follow controller, but still lose read authority on rejection.
      const refusal = get(readRefusal)
      if (refusal !== undefined) return unavailable({ reason: 'ungranted', detail: refusal })
      return latest
    })
    retained = {
      atom,
      interest,
      snapshot: data,
      controller: following,
      sync: syncData,
      publish: (value) => {
        // A refused read stays refused: a local outbox update must never
        // resurrect transcript rows the gateway stopped authorizing.
        if (latest._tag === 'Unavailable') return false
        latest = latest._tag === 'Observed' ? { ...latest, value } : observed({ value })
        registry.set(data, latest)
        return true
      },
      prefetch: () => {
        unmount ??= registry.mount(following)
        if (ended) {
          ingest.flush()
          registry.refresh(following)
        }
      },
      retry: () => {
        if (!ended && !terminalFailure) return
        // The explicit retry shows its own pending read; a renewed failure returns its verdict.
        if (latest._tag === 'Unavailable') latest = waiting
        ingest.flush()
        registry.refresh(following)
      },
      release: () => {
        unmount?.()
        unmount = undefined
      },
    }
    return retained
  }

  const agentsRetained = retain<readonly Agent[], Extract<FollowSpec, { _tag: 'Window' }>>({
    resolve: () => Effect.succeed({ _tag: 'Window', collection: 'agents', limit: 100 }),
    follow: ({ st3, spec }) =>
      st3.followWindow(spec).pipe(
        Stream.map((event) =>
          event._tag === 'Observed'
            ? {
                _tag: 'Observed' as const,
                value: event.value.items.filter((row): row is Agent => row.kind === 'agent'),
              }
            : event,
        ),
      ),
  })
  const agents = agentsRetained.atom
  const missionsRetained = retain<readonly Mission[], Extract<FollowSpec, { _tag: 'Window' }>>({
    resolve: () => Effect.succeed({ _tag: 'Window', collection: 'missions', limit: 100 }),
    follow: ({ st3, spec }) =>
      st3.followWindow(spec).pipe(
        Stream.map((event) =>
          event._tag === 'Observed'
            ? {
                _tag: 'Observed' as const,
                value: event.value.items.filter((row): row is Mission => row.kind === 'mission'),
              }
            : event,
        ),
      ),
  })
  const missions = missionsRetained.atom
  const attentionRetained = retain<readonly Attention[], Extract<FollowSpec, { _tag: 'Window' }>>({
    resolve: () => Effect.succeed({ _tag: 'Window', collection: 'attention', limit: 100 }),
    follow: ({ st3, spec }) =>
      st3.followWindow(spec).pipe(
        Stream.map((event) =>
          event._tag === 'Observed'
            ? {
                _tag: 'Observed' as const,
                value: event.value.items.filter(
                  (row): row is Attention => row.kind === 'attention',
                ),
              }
            : event,
        ),
      ),
  })
  const attention = attentionRetained.atom
  const messageSnapshot = (): Promise<ConversationPortResult<string>> =>
    runtime.runPromise(
      Effect.gen(function* () {
        const st3 = yield* St3
        const statuses = yield* SubscriptionRef.get(st3.syncStatuses)
        let freshest: typeof Snapshot.Type | undefined
        // Window snapshots fence the gateway's complete store, not just their rows.
        // Conversation frames have no Snapshot; their opaque cursors are not action fences.
        // Use the SDK's already-held evidence, excluding remote terminal-owner snapshots.
        for (const [key, status] of statuses) {
          if (!key.startsWith('window:') || status._tag !== 'Live' || status.snapshot === undefined) continue
          const decoded = Schema.decodeUnknownOption(Snapshot)(status.snapshot)
          if (Option.isSome(decoded) && (freshest === undefined || decoded.value.store_index > freshest.store_index))
            freshest = decoded.value
        }
        if (freshest !== undefined) return { _tag: 'Success' as const, value: freshest.id }
        return yield* st3.snapshot.pipe(
          Effect.map((value): ConversationPortResult<string> => ({ _tag: 'Success', value })),
          Effect.catch((failure) => Effect.succeed<ConversationPortResult<string>>({
            _tag: 'Refused', reason: 'snapshot-unavailable',
            detail: `Cannot obtain a current message snapshot: ${failure.message}`,
          })),
        )
      }),
    )
  const submitMessage = gatewayMessageSend(client)

  // Atom.family is weakly memoized. Keep the 24 recent snapshot controllers explicitly.
  const visibleConversations = new Map<string, boolean>()
  const attachments = gatewayAttachments(client, messageSnapshot)
  const conversationFamily = Atom.family((ref: string): RetainedConversation => {
    const timeline = new LiveTimeline()
    // A Sent row retires only when its identity-correlated server content is in the window.
    // Until smalltalk#1977 provides owner-side read-after-send visibility, an absent row on
    // even a later replace page proves nothing; outbox and key aliases share this feed's lifetime.
    const pending = new Map<string, PendingSend>()
    let lastSendId: string | undefined
    let painted = false
    let publishedItems: ConversationPage['items'] = []
    let changedFrom = Infinity
    const projectPage = (): ConversationPage => {
      const projection = timeline.project()
      const hadPending = pending.size > 0
      if (hadPending)
        for (const item of projection.items) pending.delete(item.id)
      const items = pending.size === 0 ? projection.items : mergeOutboxItems(projection.items, pending.values())
      if (hadPending) {
        // Timeline indices exclude interleaved outbox rows. Compare against the actual
        // last published page, including when an echo retires the final outbox row.
        let common = 0
        while (common < items.length && common < publishedItems.length && items[common] === publishedItems[common]) common += 1
        changedFrom = Math.min(changedFrom, common)
      } else changedFrom = Math.min(changedFrom, projection.changedFrom)
      return {
        items,
        ...(lastSendId === undefined ? {} : { lastSendId }),
        hasOlder: timeline.hasOlder,
        change: { from: publishedItems, index: changedFrom },
        ...(timeline.observation === undefined ? {} : { observation: timeline.observation }),
      }
    }
    const retained = retain<ConversationPage, Extract<FollowSpec, { _tag: 'Conversation' }>>({
      keepAlive: false,
      telemetryKind: 'conversation',
      explicitInterest: true,
      resolve: () => Effect.succeed({ _tag: 'Conversation', ref }),
      onCommit: (page) => {
        publishedItems = page.items
        changedFrom = Infinity
        painted = true
        if (visibleConversations.has(ref)) visibleConversations.set(ref, true)
      },
      onVisibilityChange: (visible) => {
        if (visible) visibleConversations.set(ref, painted)
        else visibleConversations.delete(ref)
      },
      follow: ({ st3, spec }) =>
        st3.followConversation(spec).pipe(
          Stream.map((event) => {
            if (event._tag !== 'Observed') return event
            ux?.().switchDataReady(ref)
            const previousSize = timeline.size
            timeline.apply(event.value)
            // Entries are identity-deduplicated by the retained timeline, not counted per chunk.
            incrDebug('Wf.conversationEntries', timeline.size - previousSize)
            return { _tag: 'Observed' as const, value: projectPage() }
          }),
        ),
    })
    const publishPending = () => {
      const page = projectPage()
      if (!retained.publish(page)) return
      publishedItems = page.items
      changedFrom = Infinity
    }
    const send: AttachmentPort['send'] = async (request) => {
      const idempotencyKey = request._tag === 'Resend' ? request.idempotencyKey : crypto.randomUUID()
      const id = `pending/${idempotencyKey}`
      const local: PendingSend = {
        item: {
          _tag: 'Text', id, role: 'user',
          text: request.parameters.content ?? '',
          attachments: request.parameters.attachments.map((attachment) => ({
            id: attachment.blob,
            mediaType: attachment.media_type,
            // The encoded wire side carries `name: null`; the view model has no null names.
            ...(typeof attachment.name === 'string' ? { name: attachment.name } : {}),
          })),
          streaming: false, at: pending.get(id)?.item.at ?? new Date().toISOString(), sendState: { _tag: 'Pending' },
        },
      }
      // A first send paints immediately: Enter must not wait for any await. An explicit
      // Resend names mail the stream may already show, so its authoritative identity is
      // reconciled before any optimistic row can duplicate it.
      if (request._tag === 'Send') {
        lastSendId = id
        pending.set(id, local)
        publishPending()
      }
      try {
        // The public device-signing contract names mail from the first 16 SHA-256 hex digits.
        // Resolve this before POST so an echo that wins the HTTP race still replaces its outbox item.
        const hash = new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(idempotencyKey)))
        const messageIds = [`message/${[...hash.slice(0, 8)].map((byte) => byte.toString(16).padStart(2, '0')).join('')}`]
        if (request._tag === 'Send') timeline.keepOwnSendId(messageIds, id)
        const submit = async () => {
          const current = await messageSnapshot()
          if (current._tag === 'Refused') return current
          return submitMessage({
            api_version: request.api_version,
            type: request.type,
            id: request.id,
            fence: { snapshot_id: current.value, subject_revisions: {} },
            parameters: request.parameters,
            idempotency_key: idempotencyKey,
          })
        }
        if (request._tag === 'Resend' && timeline.shownMessageIds().has(messageIds[0]!)) return submit()
        if (request._tag === 'Resend') {
          pending.set(id, local)
          timeline.keepOwnSendId(messageIds, id)
          publishPending()
        }
        const result = await submit()
        if (result._tag === 'Success' && result.value.status === 'rejected') {
          local.item = { ...local.item, sendState: { _tag: 'Failed', reason: 'rejected', detail: 'The message action was rejected.' } }
        } else if (result._tag === 'Refused') {
          local.item = { ...local.item, sendState: { _tag: 'Failed', reason: result.reason, detail: result.detail } }
        } else if (result._tag === 'Success' && result.value.status === 'completed') {
          const affected = result.value.affected_ids.filter((affected) => affected.startsWith('message/'))
          if (affected.length > 0) timeline.keepOwnSendId(affected, id)
          // Terminal gateway success settles the outbox row even when the mailbox echo
          // falls outside the newest window; an in-window echo still removes it.
          local.item = { ...local.item, sendState: { _tag: 'Sent' } }
        }
        publishPending()
        return result
      } catch (cause) {
        const detail = cause instanceof Error ? cause.message : String(cause)
        local.item = { ...local.item, sendState: { _tag: 'Failed', reason: 'failed', detail } }
        publishPending()
        return { _tag: 'Refused', reason: 'failed', detail }
      }
    }
    return { ...retained, send }
  })
  const recentConversations = new Map<string, RetainedConversation>()
  const retainConversation = (ref: string) => {
    const entry = conversationFamily(ref)
    recentConversations.delete(ref)
    recentConversations.set(ref, entry)
    // Visible readers independently own their mounts. The SDK admits socket follows
    // and evicts invisible ones; this LRU bounds retained client snapshot identities.
    if (recentConversations.size > 24) {
      for (const [key, candidate] of recentConversations) {
        recentConversations.delete(key)
        candidate.release()
        if (recentConversations.size <= 24) break
      }
    }
    return entry
  }
  const conversation = (ref: string) => retainConversation(ref).atom
  const conversationSync = (ref: string) => retainConversation(ref).sync
  let releaseSelection: (() => void) | undefined
  const selectConversation = (ref: string) => {
    releaseSelection?.()
    const entry = retainConversation(ref)
    const snapshot = registry.get(entry.snapshot)
    const warm = snapshot._tag === 'Observed'
    ux?.().beginSwitch({ ref, warm, slotCount: getDebug('Wf.activeFollows') })
    releaseSelection = registry.mount(entry.interest)
    if (warm) ux?.().switchDataReady(ref)
  }
  const terminalFamily = Atom.family((ref: string) => {
    const agentRef = `agent/${ref.slice('terminal/'.length)}`
    const runtimes = Atom.make((get): readonly string[] | undefined => {
      const fleet = get(agents)
      return fleet._tag === 'Observed'
        ? fleet.freshness === 'live'
          ? fleet.value.find((row) => row.id === agentRef)?.runtime_ids
          : Option.getOrUndefined(get.self<readonly string[] | undefined>())
        : undefined
    }).pipe(
      Atom.withEquality<readonly string[] | undefined>(
        (previous, next) =>
          previous === next ||
          (previous !== undefined &&
            next !== undefined &&
            previous.length === next.length &&
            previous.every((id, index) => id === next[index])),
      ),
    )
    return retain<TerminalScreen, Extract<FollowSpec, { _tag: 'Terminal' }>>({
      explicitInterest: true,
      telemetryKind: 'terminal',
      resolve: (get) => {
        const ids = get(runtimes)
        const authority = ids === undefined ? get(agents) : undefined
        return Effect.gen(function* () {
          if (authority?._tag === 'Unavailable' && authority.reason === 'ungranted')
            return yield* new AttachFailure({ authorizationRefused: true, message: authority.detail })
          if (ids === undefined)
            return yield* new AttachFailure({ message: `No live agent owns ${ref}` })
          for (const id of ids) {
            const response = yield* Effect.tryPromise({
              try: () => client.runtimesGet(id),
              catch: (error) => error instanceof ClientError
                ? new AttachFailure({ code: error.response.code, status: error.status, message: error.response.message })
                : new AttachFailure({ message: String(error) }),
            })
            const row = yield* Effect.try({
              try: () => decodeUnknownSync(Runtime)(response.value),
              catch: (error) => new AttachFailure({ message: String(error) }),
            })
            if (Option.isSome(row.terminal_id)) return { _tag: 'Terminal' as const, runtime: id }
          }
          return yield* new AttachFailure({ message: `${agentRef} has no terminal runtime` })
        })
      },
      follow: ({ st3, spec }) =>
        st3.followTerminal(spec).pipe(
          Stream.tap((event) =>
            Effect.sync(() => {
              if (event._tag === 'Observed') incrDebug('Wf.terminalUpdates')
            }),
          ),
        ),
    })
  })
  const terminal = (ref: string) => terminalFamily(ref).atom
  const terminalSync = (ref: string) => terminalFamily(ref).sync

  const connection = Atom.keepAlive(
    Atom.make((get): ConnectionState => {
      const fiber = runtime.runFork(
        Effect.flatMap(St3, (st3) =>
          st3.connection.pipe(Stream.runForEach((value) => Effect.sync(() => get.setSelf(value)))),
        ),
      )
      get.addFinalizer(() => fiber.interruptUnsafe())
      return { _tag: 'Connecting', attempt: 1 }
    }),
  )
  const gatewaySync = Atom.keepAlive(
    Atom.make((get) => {
      let current = initialFeedSync<never>(Date.now())
      const fiber = runtime.runFork(
        Effect.flatMap(St3, (st3) =>
          st3.gatewaySyncStatus.pipe(
            Stream.runForEach((status) => Effect.sync(() => {
              current = transitionFeedSync(current, status, Date.now())
              ux?.().observeSync({ key: gatewaySync, kind: 'gateway', status })
              get.setSelf(current.sync)
            })),
          ),
        ),
      )
      get.addFinalizer(() => fiber.interruptUnsafe())
      return current.sync
    }),
  )
  const grants = Atom.keepAlive(
    Atom.make((get): Grants => {
      if (readRejection !== undefined) return noGrants
      const deny = () => get.setSelf(noGrants)
      deniedReaders.add(deny)
      const fiber = runtime.runFork(
        Effect.gen(function* () {
          const st3 = yield* St3
          const discovered = yield* st3.capabilities
          const allowed = new Set(
            discovered.capabilities
              .filter((capability) => capability.state === 'granted')
              .map((capability) => capability.id),
          )
          if (readRejection !== undefined) return
          get.setSelf({
            actions:
              allowed.has('message.send') || allowed.has('work.done') ? 'granted' : 'ungranted',
            messageSend: allowed.has('message.send') ? 'granted' : 'ungranted',
            terminalInput: allowed.has('terminal.input') ? 'granted' : 'ungranted',
          })
        }),
      )
      get.addFinalizer(() => {
        deniedReaders.delete(deny)
        fiber.interruptUnsafe()
      })
      return noGrants
    }),
  )
  return {
    selectConversation,
    registry,
    ready: runtime.runPromise(Effect.map(St3, (service) => { sdk = service })),
    suspendSockets: () => sdk?.suspendSockets(),
    resumeSockets: () => {
      if (!disposed && sdk !== undefined) runtime.runFork(sdk.resumeSockets)
    },
    source: {
      mode: 'live',
      subjectReads,
      label: 'gateway',
      gateway: new URL(baseUrl).host,
      now: wallClock,
      grants,
      contentSearch: gatewayContentSearch(client),
      attachments: { ...attachments, send: (request) => retainConversation(request.parameters.to).send(request) },
      sessionTrace: (query) => runtime.runPromise(sessionTrace(query)),
      connection,
      agents,
      missions,
      attention,
      conversation,
      conversationInterest: (ref) => retainConversation(ref).interest,
      prefetchConversation: (ref) => {
        // Explicit intent never competes with the selected thread's cold first page.
        // Only the frame writer marks a real observed page as painted.
        if (visibleConversations.size === 0) return
        for (const painted of visibleConversations.values())
          if (!painted) return
        retainConversation(ref).prefetch()
      },
      retryConversation: (ref) => retainConversation(ref).retry(),
      resources,
      terminal,
      terminalInterest: (ref) => terminalFamily(ref).interest,
      terminalResize: gatewayTerminalResize(client),
      terminalHistory: unavailableTerminalHistory,
      events: Atom.make(
        unavailable({
          reason: 'unsupported',
          detail: 'Live gateway events are not available in this workbench.',
        }),
      ),
      envelope: Atom.family(() =>
        Atom.make(
          unavailable({
            reason: 'unsupported',
            detail: 'Live resource envelopes are not supported.',
          }),
        ),
      ),
      usage: undeclared,
      sync: {
        gateway: gatewaySync,
        agents: agentsRetained.sync,
        missions: missionsRetained.sync,
        attention: attentionRetained.sync,
        conversation: conversationSync,
        terminal: terminalSync,
      },
    },
    dispose: async () => {
      disposed = true
      releaseSelection?.()
      ingest.dispose()
      for (const entry of recentConversations.values()) entry.release()
      recentConversations.clear()
      registry.dispose()
      await runtime.dispose()
      setDebug('Wf.conversationEntries', 0)
    },
  }
}
