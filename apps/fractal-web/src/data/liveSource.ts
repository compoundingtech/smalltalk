import { St3Client } from '@smalltalk/st3-client'
import { Runtime, decodeUnknownSync } from '@smalltalk/st3-client/schema'
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
} from '@st3/sdk/effect'
import { Effect, Fiber, Layer, ManagedRuntime, Metric, Option, Stream } from 'effect'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'

/** One SDK runtime and one frame writer for the application's retained live projections. */
import { incrDebug, setDebug } from '../telemetry/meters.tsx'

import { LiveTimeline } from '../conversation/fromTimeline.ts'
import { undeclared } from '../monitor/source.ts'
import { gatewayResources } from '../resources/agent/source.ts'
import { instrumentFetch } from '../telemetry/transport.ts'
import { unavailableTerminalHistory } from '../terminal/historySource.ts'
import { gatewayTerminalResize } from '../terminal/terminal-resize-port.ts'
import { gatewayAttachments } from './attachmentPort.ts'
import { gatewayContentSearch } from './contentSearchPort.ts'
import { makeFrameIngest } from './frameIngest.ts'
import { nativeAgentFetch } from './nativeAgentFetch.ts'
import {
  initialFeedSync,
  observeFeedSync,
  transitionFeedSync,
  type FeedSync,
  type FeedSyncFailure,
  type FeedSyncStatus,
} from './feedSync.ts'
import {
  type ConversationPage,
  type DataSource,
  type Feed,
  type Grants,
  observed,
  unavailable,
  waiting,
  wallClock,
} from './source.ts'
import { gatewaySubjectReads, SubjectReadPort, subjectReaderFromAtom } from './subjectReadPort.ts'

/** The owned source registry and runtime teardown handle. */
export interface LiveSource {
  readonly source: DataSource
  readonly registry: AtomRegistry.AtomRegistry
  readonly dispose: () => Promise<void>
}

interface RetainedFeed<A> {
  readonly atom: Atom.Atom<Feed<A>>
  readonly interest: Atom.Atom<void>
  readonly snapshot: Atom.Atom<Feed<A>>
  readonly controller: Atom.Atom<Feed<A>>
  readonly sync: Atom.Atom<FeedSync<A>>
  readonly prefetch: () => void
  readonly release: () => void
}

/** Connect retained workbench projections through one SDK runtime and frame writer. */
export const liveSource = ({
  options,
  telemetryLayer,
}: {
  readonly options: St3Options
  /** Browser root owns sampling and observers; the SDK shares its scoped tracer/exporter. */
  readonly telemetryLayer?: Layer.Layer<never> | undefined
}): LiveSource => {
  const { baseUrl } = options
  const fetch = instrumentFetch(nativeAgentFetch({
    origin: typeof location === 'undefined' ? new URL(baseUrl).origin : location.origin,
    fetchImpl: options.fetch ?? globalThis.fetch.bind(globalThis),
  }))
  const deniedReaders = new Set<(message: string) => void>()
  const feedSyncCallbacks = new Map<string, {
    readonly requested: () => void
    readonly failed: (failure: FeedSyncFailure) => void
  }>()
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
  const client = new St3Client({
    baseUrl,
    // Remove the browser receiver workaround after smalltalk#1040 / #1247 lands.
    fetchImpl: fetch,
  })
  const registry = AtomRegistry.make()
  const readRefusal = Atom.keepAlive(Atom.make<string | undefined>(undefined))
  const resources = gatewayResources({
    baseUrl,
    fetchImpl: options.fetch ?? globalThis.fetch.bind(globalThis),
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
      onFollowSubscribeSent: (event) => {
        feedSyncCallbacks.get(event.key)?.requested()
        options.onFollowSubscribeSent?.(event)
      },
      onDiagnostics: (event) => {
        switch (event._tag) {
          case 'Connection':
            if (event.state._tag === 'Reconnecting') connectionAttempt = event.state.attempt
            setDebug('Wf.socketLive', event.state._tag === 'Live' ? 1 : 0)
            if (event.state._tag === 'Rejected') {
              for (const callbacks of feedSyncCallbacks.values())
                callbacks.failed({ _tag: 'ConnectionRejected', message: event.state.message })
            }
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
      Layer.provideMerge(telemetryLayer ?? Layer.empty),
    ),
  )
  const ingest = makeFrameIngest<() => void, () => void>({ write: ({ value }) => value() })

  const retain = <A, TSpec extends FollowSpec>({
    resolve,
    follow,
    keepAlive = true,
    onCommit,
    explicitInterest = false,
    onVisibilityChange,
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
        { _tag: 'Failed', failure: { _tag: 'ConnectionRejected', message: readRejection } },
        Date.now(),
      )
    const syncData = Atom.keepAlive(Atom.make<FeedSync<A>>(syncLatest))
    let unmount: (() => void) | undefined
    const data = Atom.make<Feed<A>>(latest)
    const following = Atom.make((get): Feed<A> => {
      let active = true
      if (readRejection !== undefined)
        latest = unavailable({ reason: 'ungranted', detail: readRejection })
      else if (latest._tag === 'Observed') latest = { ...latest, freshness: 'stale' }
      registry.set(data, latest)
      const commit = () => {
        if (active) {
          if (latest._tag === 'Observed') onCommit?.(latest.value)
          registry.set(data, latest)
          registry.set(syncData, syncLatest)
        }
      }
      const setSync = (status: FeedSyncStatus) => {
        syncLatest = transitionFeedSync(syncLatest, status, Date.now())
        ingest.accept({ key: commit, value: commit })
      }
      let syncKey: string | undefined
      let syncCallbacks: {
        readonly requested: () => void
        readonly failed: (failure: FeedSyncFailure) => void
      } | undefined
      const deny = (message: string) => {
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
      const fiber = runtime.runFork(
        Effect.gen(function* () {
          // Release the old socket slot before binding the replacement owner.
          if (previous !== undefined) yield* Fiber.interrupt(previous)
          if (readRejection !== undefined) {
            setSync({ _tag: 'Failed', failure: { _tag: 'ConnectionRejected', message: readRejection } })
            return
          }
          const st3 = yield* St3
          const boundSpec = yield* resolving
          spec = boundSpec
          syncKey = followKey(boundSpec)
          const callbacks = {
            requested: () => setSync({ _tag: 'Requested' }),
            failed: (failure: FeedSyncFailure) =>
              setSync({ _tag: 'Failed', failure }),
          }
          syncCallbacks = callbacks
          feedSyncCallbacks.set(syncKey, callbacks)
          yield* st3.setVisible(boundSpec, visible)
          yield* follow({ st3, spec: boundSpec }).pipe(
            Stream.runForEach((event) =>
              Effect.sync(() => {
                if (!active || readRejection !== undefined) return
                if (event._tag === 'Observed') {
                  latest = observed({ value: event.value })
                  syncLatest = observeFeedSync(syncLatest, event.value, Date.now())
                } else if (event._tag === 'Failed') {
                  syncLatest = transitionFeedSync(syncLatest, { _tag: 'Failed', failure: event.error }, Date.now())
                  terminalFailure = true
                  // Keep only previously decoded rows, never fabricate missing claims.
                  // The daemon's integrity failure stays visible and remains non-retryable.
                  const incompleteHistory =
                    event.error._tag === 'Rejected' &&
                    event.error.code === 'timeline-history-incomplete'
                  latest =
                    spec?._tag === 'Conversation' && incompleteHistory && latest._tag === 'Observed'
                      ? {
                          ...latest,
                          freshness: 'stale',
                          error: {
                            reason: 'failed',
                            detail: event.error.message,
                          },
                        }
                      : unavailable({
                          reason:
                            event.error._tag === 'Rejected' && !incompleteHistory
                              ? 'ungranted'
                              : 'failed',
                          detail: event.error.message,
                        })
                } else {
                  syncLatest = transitionFeedSync(syncLatest, {
                    _tag: 'Stale',
                    ...(event.code === undefined ? {} : { code: event.code }),
                    ...(event.message === undefined ? {} : { message: event.message }),
                  }, Date.now())
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
              latest = unavailable({ reason: 'failed', detail: error.message })
              ingest.accept({ key: commit, value: commit })
            }),
          ),
          Effect.ensuring(
            Effect.sync(() => {
              if (active) ended = true
            }),
          ),
        ),
      )
      previousFiber = fiber
      get.addFinalizer(() => {
        active = false
        deniedReaders.delete(deny)
        if (syncKey !== undefined && feedSyncCallbacks.get(syncKey) === syncCallbacks)
          feedSyncCallbacks.delete(syncKey)
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
      if (ended && !terminalFailure) {
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
      prefetch: () => {
        unmount ??= registry.mount(following)
        if (ended && !terminalFailure) {
          ingest.flush()
          registry.refresh(following)
        }
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

  // Atom.family is weakly memoized. Keep the 24 recent snapshot controllers explicitly.
  const visibleConversations = new Map<string, boolean>()
  const conversationFamily = Atom.family((ref: string) => {
    const timeline = new LiveTimeline()
    let painted = false
    let publishedItems: ConversationPage['items'] = []
    let changedFrom = Infinity
    return retain<ConversationPage, Extract<FollowSpec, { _tag: 'Conversation' }>>({
      keepAlive: false,
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
            const previousSize = timeline.size
            timeline.apply(event.value)
            // Entries are identity-deduplicated by the retained timeline, not counted per chunk.
            incrDebug('Wf.conversationEntries', timeline.size - previousSize)
            const projection = timeline.project()
            // Coalesced deltas share the earliest dirty suffix of the published frame.
            changedFrom = Math.min(changedFrom, projection.changedFrom)
            const change = { from: publishedItems, index: changedFrom }
            return {
              _tag: 'Observed' as const,
              value: {
                items: projection.items,
                hasOlder: timeline.hasOlder,
                change,
                ...(timeline.observation === undefined ? {} : { observation: timeline.observation }),
              },
            }
          }),
        ),
    })
  })
  const recentConversations = new Map<string, RetainedFeed<ConversationPage>>()
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
      resolve: (get) => {
        const ids = get(runtimes)
        return Effect.gen(function* () {
          if (ids === undefined)
            return yield* new AttachFailure({ message: `No live agent owns ${ref}` })
          for (const id of ids) {
            const response = yield* Effect.tryPromise({
              try: () => client.runtimesGet(id),
              catch: (error) => new AttachFailure({ message: String(error) }),
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
    registry,
    source: {
      mode: 'live',
      subjectReads,
      label: 'gateway',
      gateway: new URL(baseUrl).host,
      now: wallClock,
      grants,
      contentSearch: gatewayContentSearch(client),
      attachments: gatewayAttachments(client),
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
        agents: agentsRetained.sync,
        missions: missionsRetained.sync,
        attention: attentionRetained.sync,
        conversation: conversationSync,
        terminal: terminalSync,
      },
    },
    dispose: async () => {
      ingest.dispose()
      for (const entry of recentConversations.values()) entry.release()
      recentConversations.clear()
      registry.dispose()
      await runtime.dispose()
      setDebug('Wf.conversationEntries', 0)
    },
  }
}
