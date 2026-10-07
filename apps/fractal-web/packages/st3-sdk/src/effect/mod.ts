/**
 * `@st3/sdk/effect`: the st client-v0 transport for wf, as one Effect service.
 *
 * One collections socket carries every follow (`socket.ts`). Each follow is a scoped
 * `Stream<FollowEvent<A>>` decoded tolerantly at this edge with smalltalk's generated codecs, so
 * live drift never breaks the page. Resync and transient failures mark a follow stale; retries
 * coalesce per follow and wait one second before subscribing afresh.
 * Follows hold socket slots under the admission rules in `admission.ts`; an evicted follow's
 * stream ends with a final `Stale`.
 *
 * `messageSend` uses the generated HTTP action client and decodes its acknowledgement; it does
 * not retry ambiguous delivery. Daemon refusals retain the complete error envelope and HTTP
 * status, separately from transport failures. `snapshot` reads fresh capabilities over HTTP,
 * bypassing cached discovery so the composer's fence refresh is authoritative.
 */
import {
  ClientError,
  type ActionOf,
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
import * as Queue from 'effect/Queue'
import * as Stream from 'effect/Stream'
import * as SubscriptionRef from 'effect/SubscriptionRef'

import { makeAdmission } from './admission.ts'
import { type ConnectionState, type St3Diagnostic, makeChannel } from './socket.ts'
import { makeWindowIngest, type ProcessedWindow } from './windowIngest.ts'

// Arrangement windows use the generated owner-scoped subscribeArrangements API.
type CollectionName = Parameters<CollectionStream['subscribe']>[1]

export type { ConnectionState, St3Diagnostic } from './socket.ts'

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
export class AttachFailure extends Data.TaggedError('Attach')<{ readonly message: string }> {}

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
  readonly replace: boolean
  readonly hasMore: boolean
  readonly entries: readonly (TimelineEntry | UnrecognizedEntry)[]
}

/** Scoped collection follows and gateway state carried by one shared socket. */
export class St3 extends Context.Service<
  St3,
  {
    readonly connection: Stream.Stream<ConnectionState>
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
  }
>()('@st3/sdk/St3') {}

/** Gateway configuration and transport seams for the scoped SDK layer. */
export interface St3Options {
  /** Gateway origin; the browser passes its own origin (the dev proxy or the serving gateway). */
  readonly baseUrl: string
  /** Socket slots: the daemon's subscription cap (8 on current daemons). */
  readonly maxFollows: number
  /** Test seam: the WebSocket the collections socket opens. */
  readonly socket?: CollectionSocketFactory
  /** Test seam: HTTP reads and actions. */
  readonly fetch?: typeof fetch
  /** Authoritative transitions and frame work, without coupling the SDK to a metrics library. */
  readonly onDiagnostics?: (event: St3Diagnostic) => void
  /** Called after a follow's subscribe command is sent on an open WebSocket; includes resubscribe sends. */
  readonly onSubscribeSent?: (id: string) => void
  /** Actual-send callback with the canonical followKey; includes queued-open and resubscribe sends. */
  readonly onFollowSubscribeSent?: (event: FollowSubscribeSent) => void
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

/** A frame error without a code that names a subscription is today's daemon subscription cap. */
const isSubscriptionLimit = (frame: Extract<CollectionFrame, { kind: 'error' }>) =>
  frame.code === 'subscription-limit' ||
  (frame.code === undefined && frame.collection === undefined)

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

const make = (options: St3Options) =>
  Effect.gen(function* () {
    const client = new St3Client({
      baseUrl: options.baseUrl,
      // Remove the browser receiver workaround after smalltalk#1040 / #1247 lands.
      fetchImpl: options.fetch ?? globalThis.fetch.bind(globalThis),
    })
    const followKeys = new Map<string, string>()
    const channel = yield* makeChannel({
      client,
      ...(options.socket === undefined ? {} : { socket: options.socket }),
      ...(options.onDiagnostics === undefined ? {} : { onDiagnostics: options.onDiagnostics }),
      ...(options.onSubscribeSent === undefined && options.onFollowSubscribeSent === undefined
        ? {}
        : {
            onSubscribeSent: (id: string) => {
              options.onSubscribeSent?.(id)
              const key = followKeys.get(id)
              if (key !== undefined) options.onFollowSubscribeSent?.({ id, key })
            },
          }),
    })
    const admission = makeAdmission<string>({ cap: options.maxFollows })
    /** The open follow per key: its socket id and how to evict it. */
    const open = new Map<string, { readonly id: string; readonly evict: () => void }>()
    const reportFollows = () =>
      options.onDiagnostics?.({ _tag: 'Follows', active: open.size, cap: admission.cap() })
    reportFollows()
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
      readonly makeProtocol: (observe: (value: A) => void) => FollowProtocol<A>
    }) =>
      Stream.callback<FollowEvent<A>>((queue) =>
        Effect.gen(function* () {
          const key = followKey(spec)
          const id = `f${(nextId += 1)}`
          followKeys.set(id, key)
          const emit = (event: FollowEvent<A>) => {
            Queue.offerUnsafe(queue, event)
          }
          const protocol = makeProtocol((value) => emit({ _tag: 'Observed', value }))
          const fail = (error: FollowFailure) => {
            emit({ _tag: 'Failed', error })
            stop()
          }
          const stop = () => {
            followKeys.delete(id)
            protocol.reset()
            channel.unregister(id)
            if (open.get(key)?.id === id) {
              open.delete(key)
              admission.close(key)
              reportFollows()
            }
            Queue.endUnsafe(queue)
          }
          const admitted = admission.open(key)
          if (admitted._tag === 'Full') {
            fail(new SubscriptionLimit({ cap: admission.cap() }))
            return
          }
          evict(admitted.evict)
          open.set(key, {
            id,
            evict: () => {
              emit({ _tag: 'Stale' })
              protocol.reset()
              channel.unregister(id)
              open.delete(key)
              reportFollows()
              Queue.endUnsafe(queue)
            },
          })
          reportFollows()
          yield* Effect.addFinalizer(() => Effect.sync(stop))

          const scope = yield* Effect.scope
          const context = yield* Effect.context()
          const subscribe = (stream: CollectionStream) =>
            protocol
              .subscribe({ stream, id })
              .pipe(Effect.catch((error) => Effect.sync(() => fail(error))))
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
              onDrop: () => {
                protocol.reset()
                emit({ _tag: 'Stale' })
              },
              onFrame: (frame) => {
                if (frame.kind === 'resync') {
                  options.onDiagnostics?.({ _tag: 'Resync' })
                  protocol.reset()
                  emit({
                    _tag: 'Stale',
                    ...(frame.code === undefined ? {} : { code: frame.code }),
                    ...(frame.message === undefined ? {} : { message: frame.message }),
                  })
                  Effect.runForkWith(context)(retryLater)
                  return
                }
                if (frame.kind === 'error') {
                  if (isSubscriptionLimit(frame)) {
                    const retried = admission.serverLimit(key)
                    reportFollows()
                    if (retried._tag === 'Full') {
                      fail(new SubscriptionLimit({ cap: admission.cap() }))
                      return
                    }
                    evict(retried.evict)
                    Effect.runForkWith(context)(channel.resubscribe(id))
                    return
                  }
                  if (isTransientCode(frame.code)) {
                    protocol.reset()
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
                      ? new AttachFailure({ message: frame.message })
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
                if (value !== undefined) emit({ _tag: 'Observed', value })
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
            publish: observe,
            ...(options.onDiagnostics === undefined
              ? {}
              : {
                  onSlice: (elapsedMs: number) =>
                    options.onDiagnostics?.({ _tag: 'Decode', elapsedMs }),
                }),
            decode: (row) => {
              try {
                return decodeResource(row)
              } catch (error) {
                console.warn(`st3 ${spec.collection} row did not decode`, error)
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
            return {
              replace: frame.replace,
              hasMore: frame.has_more ?? false,
              entries: frame.items.flatMap<TimelineEntry | UnrecognizedEntry>((raw) => {
                try {
                  return [decodeEntry(raw)]
                } catch {
                  return unrecognized(raw) ?? []
                }
              }),
            }
          },
          reset: () => {},
        }),
      })

    /** `runtimesGet` → attach fenced on the runtime incarnation → subscribe with the capability. */
    const attachTerminal = (runtimeRef: string) =>
      Effect.gen(function* () {
        const read = yield* Effect.tryPromise({
          try: () => client.runtimesGet(runtimeRef),
          catch: (error) => new AttachFailure({ message: errorMessage(error) }),
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
          catch: (error) => new AttachFailure({ message: errorMessage(error) }),
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
      capabilities: Effect.tryPromise({
        try: () => client.discover(),
        catch: (error) => new Rejected({ code: undefined, message: errorMessage(error) }),
      }).pipe(
        Effect.flatMap((envelope) =>
          Effect.try({
            try: () => decodeCapabilities(envelope.value),
            catch: (error) => new Rejected({ code: undefined, message: errorMessage(error) }),
          }),
        ),
      ),
      snapshot: Effect.tryPromise({
        try: () => client.capabilities(),
        catch: actionFailure,
      }).pipe(
        Effect.flatMap((envelope) =>
          Effect.try({
            try: () => decodeSnapshot(envelope.snapshot).id,
            catch: actionFailure,
          }),
        ),
        Effect.withSpan('st3.snapshot'),
      ),
      messageSend: Effect.fn('st3.messageSend')((request: MessageSendInput) =>
        Effect.tryPromise({
          try: () => {
            const { tags, ...parameters } = request.parameters
            return client.messageSend({
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
        Effect.sync(() => admission.setVisible({ key: followKey(spec), visible })),
    })
  })

/** The SDK for the life of the layer: one socket, opened on build, closed on release. */
export const St3Live = (options: St3Options) => Layer.effect(St3, make(options))
