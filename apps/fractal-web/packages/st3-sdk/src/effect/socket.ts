/**
 * The one collections socket: connect, route frames by subscription id, reconnect.
 *
 * At most one socket is alive. When it ends, every registered subscription hears `onDrop`
 * (its follow turns stale), the connection reconnects with capped exponential backoff, and every
 * subscription still registered subscribes again: the new socket's snapshots are authoritative.
 * Each registration owns its subscribe fiber; unregistering or reconnecting cancels any pending
 * attach before it can send another command. A 401/403 stops reconnecting.
 */
import {
  ClientError,
  type St3Client,
  type Capabilities,
  type EnvelopeOf,
  type CollectionFrame,
  type CollectionName,
  type CollectionSocketFactory,
  type CollectionStream,
} from '@smalltalk/st3-client'
import * as Cause from 'effect/Cause'
import * as Data from 'effect/Data'
import * as Deferred from 'effect/Deferred'
import * as Effect from 'effect/Effect'
import * as Exit from 'effect/Exit'
import type * as Fiber from 'effect/Fiber'
import * as Result from 'effect/Result'
import type * as Scope from 'effect/Scope'
import * as SubscriptionRef from 'effect/SubscriptionRef'
import type { SyncStatus } from './sync-status.ts'

/** The gateway connection as the UI shows it. */
export type ConnectionState =
  | { readonly _tag: 'Idle' }
  | { readonly _tag: 'Connecting'; readonly attempt: number }
  | { readonly _tag: 'Live' }
  | { readonly _tag: 'Reconnecting'; readonly attempt: number; readonly issue: string; readonly nextAt?: number }
  | { readonly _tag: 'Rejected'; readonly message: string; readonly code?: string }
  | { readonly _tag: 'Closed' }

/** Optional transport observations; the SDK does not depend on a telemetry backend. */
export type St3Diagnostic =
  | { readonly _tag: 'Connection'; readonly state: ConnectionState }
  /** Locally admitted slots; neither socket send nor server admission/first observation. */
  | {
      readonly _tag: 'Follows'
      readonly active: number
      readonly cap: number
      readonly conversationSlots: number | undefined
    }
  | { readonly _tag: 'Frame' }
  | { readonly _tag: 'Resync' }
  /** An actual reconnect or live-socket resubscribe attempt, not a scheduled timer. */
  | { readonly _tag: 'Retry' }
  /** Synchronous protocol decoding/projection of the last routed data frame. */
  | { readonly _tag: 'Decode'; readonly elapsedMs: number }
  /** A rejected collection row; reported once while the same bad revision remains in the window.
   * Contains no row contents, exception objects or decoder messages.
   */
  | {
      readonly _tag: 'RowDecode'
      readonly collection: CollectionName
      readonly rowId: string
      readonly revision: string
    }

/** One subscription on the socket, keyed by its registration id. */
export interface Subscriber {
  /**
   * Send the subscribe command on `stream` under wire id `id`; runs on register, after every
   * reconnect and on every resubscribe. Each run gets a fresh wire id (its generation).
   */
  readonly subscribe: (args: { readonly stream: CollectionStream; readonly id: string }) => Effect.Effect<void>
  /** A frame addressed to this subscription's current generation. */
  readonly onFrame: (frame: CollectionFrame) => void
  /** The socket ended; a fresh subscribe follows once a new one is live. */
  readonly onDrop: () => void
  /** The credential was refused permanently; end this follow instead of waiting for a retry. */
  readonly onRejected: (message: string, code?: string) => void
}

/** Subscription routing and connection state for the shared collections socket. */
export interface Channel {
  readonly connection: SubscriptionRef.SubscriptionRef<ConnectionState>
  readonly syncStatus: SubscriptionRef.SubscriptionRef<SyncStatus>
  /** A subscription has successfully decoded a data frame from this socket. */
  readonly onDecodedFrame: () => void
  /** Register and, when a socket is live, subscribe now. */
  readonly register: (args: {
    readonly id: string
    readonly subscriber: Subscriber
  }) => Effect.Effect<void>
  /** Unsubscribe on the live socket (if any) and stop routing frames to `id`. */
  readonly unregister: (id: string) => void
  /** Subscribe `id` again on the live socket under a fresh generation (a fresh terminal attach, say). */
  readonly resubscribe: (id: string) => Effect.Effect<void>
}

const backoffMillis = (attempt: number) => Math.min(30_000, 500 * 2 ** Math.max(0, attempt - 1))

const isRejection = (error: unknown): error is ClientError =>
  error instanceof ClientError && (error.status === 401 || error.status === 403)

/** Open the channel for the life of the surrounding scope; closing the scope closes the socket. */
export const makeChannel = ({
  client,
  socket,
  onDiagnostics,
  onSubscribeSent,
  onCapabilities,
  onProbeFailure,
}: {
  readonly client: St3Client
  readonly socket?: CollectionSocketFactory
  readonly onDiagnostics?: (event: St3Diagnostic) => void
  /** A subscribe command for registration `id` went out under generation wire id `wire`. */
  readonly onSubscribeSent?: (sent: { readonly id: string; readonly wire: string }) => void
  /** The connect probe's capabilities response envelope, before the socket is opened. */
  readonly onCapabilities?: (envelope: EnvelopeOf<Capabilities>) => void
  /** A completed failed probe (including permanent 401/403), before rejection/backoff. */
  readonly onProbeFailure?: () => void
}): Effect.Effect<Channel, never, Scope.Scope> =>
  Effect.gen(function* () {
    const scope = yield* Effect.scope
    const connection = yield* SubscriptionRef.make<ConnectionState>({ _tag: 'Idle' })
    const syncStatus = yield* SubscriptionRef.make<SyncStatus>({ _tag: 'Connecting', attempt: 1, since: Date.now() })
    let gatewayStatus: SyncStatus = { _tag: 'Connecting', attempt: 1, since: Date.now() }
    let lastLiveAt: number | undefined
    const setSync = (status: SyncStatus) => {
      gatewayStatus = status
      return SubscriptionRef.set(syncStatus, status)
    }
    onDiagnostics?.({ _tag: 'Connection', state: { _tag: 'Idle' } })
    const setConnection = (state: ConnectionState) =>
      SubscriptionRef.set(connection, state).pipe(
        Effect.tap(() => Effect.sync(() => onDiagnostics?.({ _tag: 'Connection', state }))),
        Effect.tap(() => {
          const now = Date.now()
          switch (state._tag) {
            case 'Idle':
            case 'Connecting':
              return setSync({ _tag: 'Connecting', attempt: state._tag === 'Idle' ? 1 : state.attempt, since: now })
            case 'Live':
              // Open proves transport reachability, not decoded-data freshness.
              return setSync({ _tag: 'Requested', since: now })
            case 'Reconnecting':
              return setSync({
                _tag: 'Stale',
                reason: state.nextAt === undefined
                  ? { _tag: 'Unknown' }
                  : { _tag: 'Reconnecting', attempt: state.attempt, nextAt: state.nextAt, issue: state.issue },
                ...(lastLiveAt === undefined ? {} : { lastLiveAt }),
              })
            case 'Rejected':
              return setSync({
                _tag: 'Failed',
                cause: state.code === undefined || state.code.length === 0
                  ? { _tag: 'Unknown' }
                  : { _tag: 'Server', code: state.code, message: state.message },
              })
            case 'Closed':
              return setSync({ _tag: 'Stale', reason: { _tag: 'Unknown' }, ...(lastLiveAt === undefined ? {} : { lastLiveAt }) })
          }
        }),
        Effect.tap(() => Effect.sync(() => {
          if (state._tag === 'Rejected')
            for (const entry of subscribers.values()) entry.subscriber.onRejected(state.message, state.code)
        })),
      )
    const subscribers = new Map<
      string,
      {
        readonly subscriber: Subscriber
        fiber: Fiber.Fiber<void> | undefined
        generation: number
        /** The current generation's wire id and the socket it was issued on. */
        wire: { readonly id: string; readonly stream: CollectionStream } | undefined
      }
    >()
    /**
     * Registration id per current-generation wire id. Resubscribes reuse no wire id, so a frame
     * the gateway queued for a superseded generation can never be read as the current one's.
     */
    const wires = new Map<string, string>()
    let live: CollectionStream | undefined

    const subscribe = (id: string) =>
      Effect.gen(function* () {
        const entry = subscribers.get(id)
        if (entry === undefined || live === undefined) return
        entry.fiber?.interruptUnsafe()
        if (entry.wire !== undefined) {
          wires.delete(entry.wire.id)
          // Release the superseded generation on its own socket; a dead socket held nothing.
          if (entry.wire.stream === live) live.unsubscribe(entry.wire.id)
        }
        entry.generation += 1
        const wire = entry.generation === 1 ? id : `${id}.${entry.generation}`
        entry.wire = { id: wire, stream: live }
        wires.set(wire, id)
        entry.fiber = yield* Effect.forkIn(entry.subscriber.subscribe({ stream: live, id: wire }), scope)
      })

    const dispatch = (frame: CollectionFrame) => {
      onDiagnostics?.({ _tag: 'Frame' })
      const id = 'id' in frame ? frame.id : undefined
      if (id === undefined) {
        // A socket-level error names no subscription: the command itself was malformed.
        if (frame.kind === 'error') console.warn(`st collections socket: ${frame.message}`)
        return
      }
      const owner = wires.get(id)
      if (owner !== undefined) subscribers.get(owner)?.subscriber.onFrame(frame)
    }

    /** One socket's life: resolves with the reason it ended. */
    const runSocket = Effect.gen(function* () {
      const ended = Deferred.makeUnsafe<Error | undefined>()
      const opened = Deferred.makeUnsafe<boolean>()
      let socketEnded = false
      const stream = yield* Effect.tryPromise(() =>
        client.collectionStream({
          onFrame: dispatch,
          onOpen: () => { Deferred.doneUnsafe(opened, Exit.succeed(true)) },
          onCommandSent: (command) => {
            if (command.kind !== 'subscribe') return
            const owner = wires.get(command.id)
            if (owner !== undefined) onSubscribeSent?.({ id: owner, wire: command.id })
          },
          onEnd: (error) => {
            socketEnded = true
            Deferred.doneUnsafe(ended, Exit.succeed(error))
            Deferred.doneUnsafe(opened, Exit.succeed(false))
          },
          ...(socket === undefined ? {} : { socket }),
        }),
      )
      return yield* Effect.gen(function* () {
        const didOpen = yield* Deferred.await(opened)
        if (!didOpen || socketEnded) return 'the collections socket ended before opening'
        live = stream
        yield* setConnection({ _tag: 'Live' })
        for (const id of subscribers.keys()) yield* subscribe(id)
        const error = yield* Deferred.await(ended)
        live = undefined
        for (const entry of subscribers.values()) {
          entry.fiber?.interruptUnsafe()
          entry.fiber = undefined
          entry.subscriber.onDrop()
        }
        return error?.message ?? 'the collections socket closed'
      }).pipe(
        // Own the acquired socket while waiting for both open and end; scope interruption can
        // happen before `live` is set, so channel finalization alone cannot close it.
        Effect.ensuring(
          Effect.sync(() => {
            if (live === stream) live = undefined
            stream.close()
          }),
        ),
      )
    })

    /** `attempt` counts tries since the last live socket; the first try of a fresh start is `Connecting`. */
    const connect = ({
      attempt,
      issue,
    }: {
      readonly attempt: number
      readonly issue: string | undefined
    }): Effect.Effect<void> =>
      Effect.gen(function* () {
        if (issue !== undefined) onDiagnostics?.({ _tag: 'Retry' })
        yield* setConnection(
          issue === undefined
            ? { _tag: 'Connecting', attempt }
            : { _tag: 'Reconnecting', attempt, issue, nextAt: Date.now() },
        )
        // An HTTP read first: the browser cannot see a refused WebSocket upgrade's status.
        const probe = yield* Effect.result(
          Effect.tryPromise({
            try: () => client.capabilities(),
            catch: (cause) =>
              new ProbeFailure({
                cause,
                message: cause instanceof Error ? cause.message : String(cause),
              }),
          }),
        )
        if (Result.isFailure(probe)) {
          onProbeFailure?.()
          const error = probe.failure
          if (isRejection(error.cause)) {
            yield* setConnection({ _tag: 'Rejected', message: error.message, code: error.cause.response.code })
            return
          }
          const reason = error.message
          yield* setConnection({ _tag: 'Reconnecting', attempt, issue: reason, nextAt: Date.now() + backoffMillis(attempt) })
          yield* Effect.sleep(backoffMillis(attempt))
          return yield* connect({ attempt: attempt + 1, issue: reason })
        }
        if (Result.isSuccess(probe)) onCapabilities?.(probe.success)
        const ended = yield* runSocket.pipe(
          Effect.catchCause((cause) => Effect.succeed(String(Cause.squash(cause)))),
        )
        yield* setConnection({ _tag: 'Reconnecting', attempt: 1, issue: ended, nextAt: Date.now() + backoffMillis(1) })
        yield* Effect.sleep(backoffMillis(1))
        return yield* connect({ attempt: 2, issue: ended })
      })

    yield* Effect.forkIn(connect({ attempt: 1, issue: undefined }), scope)
    yield* Effect.addFinalizer(() =>
      Effect.gen(function* () {
        live?.close()
        live = undefined
        for (const entry of subscribers.values()) entry.fiber?.interruptUnsafe()
        subscribers.clear()
        wires.clear()
        yield* setConnection({ _tag: 'Closed' })
      }),
    )

    return {
      connection,
      syncStatus,
      onDecodedFrame: () => {
        if (live === undefined || gatewayStatus._tag === 'Live') return
        lastLiveAt = Date.now()
        Effect.runFork(setSync({ _tag: 'Live', since: lastLiveAt }))
      },
      register: ({ id, subscriber }) =>
        Effect.gen(function* () {
          const state = yield* SubscriptionRef.get(connection)
          if (state._tag === 'Rejected') {
            subscriber.onRejected(state.message, state.code)
            return
          }
          subscribers.set(id, { subscriber, fiber: undefined, generation: 0, wire: undefined })
          yield* subscribe(id)
        }),
      unregister: (id) => {
        const entry = subscribers.get(id)
        if (entry === undefined) return
        subscribers.delete(id)
        // This synchronous callback must cancel before an HTTP attach can send a subscribe.
        entry.fiber?.interruptUnsafe()
        if (entry.wire === undefined) return
        wires.delete(entry.wire.id)
        if (entry.wire.stream === live) live.unsubscribe(entry.wire.id)
      },
      resubscribe: (id) =>
        Effect.suspend(() => {
          if (live === undefined || !subscribers.has(id)) return Effect.void
          onDiagnostics?.({ _tag: 'Retry' })
          return subscribe(id)
        }),
    } satisfies Channel
  })

/** A failed gateway probe, retaining the transport cause for rejection classification. */
class ProbeFailure extends Data.TaggedError('ProbeFailure')<{
  readonly cause: unknown
  readonly message: string
}> {}
