/**
 * Adopt the collections socket `index.html` opens before the main bundle evaluates.
 *
 * The inline early-connect script opens one socket and sends the fleet roster subscribe under its
 * own id, buffering frames (bounded) in memory until the SDK takes the handle. Taking transfers
 * ownership: the inline script detaches and stops its idle deadline, and the returned
 * `EarlySocket` belongs to the caller's scope (closing it closes the socket). The first socket the
 * SDK opens is this one; the SDK's identical subscribe is answered by the early subscription
 * instead of being sent again, its buffered frames are replayed under the SDK's id, and later
 * commands and frames for that id are translated. Nothing is persisted.
 *
 * Every holding state is bounded: frames and characters are capped, an unconsumed socket closes
 * after a deadline, and an early subscription the SDK never claims is unsubscribed after a
 * deadline, so its server slot and buffer never outlive the page's actual interest.
 */
import type { CollectionSocket } from '@smalltalk/st3-client'
import * as Option from 'effect/Option'
import * as Schema from 'effect/Schema'

import { isValidTraceparent } from './trace.ts'

/** The global the inline script in `index.html` publishes its handle under. */
export const EARLY_COLLECTIONS_GLOBAL = '__wfEarlyCollections'

/** Buffer bounds shared with the inline script. */
export const EARLY_MAX_FRAMES = 64
export const EARLY_MAX_CHARS = 4 * 1024 * 1024

const Millis = Schema.Number.check(Schema.isFinite())

/** The inline script's subscribe command; only the fleet roster window is sent early. */
const EarlyCommand = Schema.Struct({
  kind: Schema.Literal('subscribe'),
  id: Schema.String,
  collection: Schema.Literal('agents'),
  limit: Schema.Literal(100),
}).annotate({ identifier: 'St3.EarlyCollections.Command' })

/** The data a taken handle carries; the socket itself is checked separately. */
const EarlyTaken = Schema.Struct({
  id: Schema.String.check(Schema.isPattern(/^wf-early-[0-9a-f]{16}$/)),
  command: EarlyCommand,
  traceparent: Schema.String.check(Schema.makeFilter(isValidTraceparent)),
  /** `performance.now()` when the inline script started. */
  startedAt: Millis,
  /** `performance.now()` when the early subscribe was handed to the socket, if it was. */
  subscribeSentAt: Schema.optional(Millis),
  frames: Schema.Array(Schema.String).check(Schema.isMaxLength(EARLY_MAX_FRAMES)),
}).annotate({ identifier: 'St3.EarlyCollections.Taken' })
type EarlyTaken = typeof EarlyTaken.Type

const EarlyBootstrap = Schema.Struct({
  traceparent: EarlyTaken.fields.traceparent,
  startedAt: Millis,
}).annotate({ identifier: 'St3.EarlyCollections.Bootstrap' })

/** The browser socket surface the early script hands over. */
interface RawSocket {
  readonly readyState: number
  onopen: ((event: unknown) => void) | null
  onmessage: ((event: { readonly data: unknown }) => void) | null
  onclose: ((event: { readonly code: number; readonly reason: string }) => void) | null
  onerror: ((event: unknown) => void) | null
  send(data: string): void
  close(code?: number, reason?: string): void
}

const isRawSocket = (value: unknown): value is RawSocket =>
  typeof value === 'object' &&
  value !== null &&
  'readyState' in value &&
  typeof value.readyState === 'number' &&
  'send' in value &&
  typeof value.send === 'function' &&
  'close' in value &&
  typeof value.close === 'function'

const OPEN = 1

/** The early page-load trace: the reload root may adopt it so early socket work correlates. */
export interface EarlyBootstrap {
  readonly traceparent: string
  /** `performance.now()` when the inline script started. */
  readonly startedAt: number
}

/** Read the early bootstrap context without taking ownership of the socket. */
export const peekEarlyBootstrap = (target: object = globalThis): EarlyBootstrap | undefined =>
  Option.getOrUndefined(
    Schema.decodeUnknownOption(EarlyBootstrap)(Reflect.get(target, EARLY_COLLECTIONS_GLOBAL)),
  )

/** An SDK subscribe answered by the early subscription. */
export interface EarlyAdoption {
  /** The early subscribe's own context; its frame and the upgrade carried it. */
  readonly traceparent: string
  /** `performance.now()` when the early subscribe was handed to the socket. */
  readonly subscribeSentAt: number
}

export interface EarlySocket {
  /** The adoption an identical subscribe would receive now, if any. */
  readonly wouldAdopt: (command: Readonly<Record<string, unknown>>) => EarlyAdoption | undefined
  /** Socket factory seam: the early socket once, or `undefined` once it cannot be adopted. */
  readonly consume: () => CollectionSocket | undefined
  /** Release the socket if still held; idempotent. */
  readonly close: () => void
}

export interface EarlySocketBounds {
  /** Close an unconsumed socket after this long. */
  readonly consumeDeadlineMs?: number
  /** Unsubscribe the early subscription when no identical SDK subscribe claims it by then. */
  readonly claimDeadlineMs?: number
  readonly maxFrames?: number
  readonly maxChars?: number
}

/** Subscription identity: every field but `id` and `trace`, order-insensitive. */
const subscriptionKey = (command: Readonly<Record<string, unknown>>) =>
  JSON.stringify(
    Object.keys(command)
      .filter((key) => key !== 'id' && key !== 'trace' && command[key] !== undefined)
      .sort()
      .map((key) => [key, command[key]]),
  )

/**
 * Take the early socket from `target`. Returns `undefined` when the script did not run, already
 * ended, or published a handle this SDK cannot verify (which is then closed).
 */
export const takeEarlyCollections = (
  target: object = globalThis,
  bounds: EarlySocketBounds = {},
): EarlySocket | undefined => {
  const handle: unknown = Reflect.get(target, EARLY_COLLECTIONS_GLOBAL)
  if (typeof handle !== 'object' || handle === null || !('take' in handle)) return undefined
  const take = handle.take
  if (typeof take !== 'function') return undefined
  const taken: unknown = take.call(handle)
  if (typeof taken !== 'object' || taken === null || !('socket' in taken)) return undefined
  const socket = taken.socket
  const decoded = Schema.decodeUnknownOption(EarlyTaken)(taken)
  if (!isRawSocket(socket)) return undefined
  if (Option.isNone(decoded) || decoded.value.command.id !== decoded.value.id) {
    socket.close(1000)
    return undefined
  }
  return makeEarlySocket(socket, decoded.value, bounds)
}

const makeEarlySocket = (
  socket: RawSocket,
  taken: EarlyTaken,
  {
    consumeDeadlineMs = 15_000,
    claimDeadlineMs = 10_000,
    maxFrames = EARLY_MAX_FRAMES,
    maxChars = EARLY_MAX_CHARS,
  }: EarlySocketBounds,
): EarlySocket => {
  const earlyKey = subscriptionKey(taken.command)
  // Frames are compact serde JSON, so the subscription id appears verbatim; the id is random.
  const earlyToken = `"id":${JSON.stringify(taken.id)}`
  let subscribeSentAt = taken.subscribeSentAt
  /** Held: buffering for the early id. Claimed: translating for `sdkId`. Released: dropping. */
  let state: 'held' | 'claimed' | 'released' = 'held'
  let sdkId: string | undefined
  let consumed: CollectionSocket | undefined
  let closed = false
  let buffered = [...taken.frames]
  let chars = buffered.reduce((total, frame) => total + frame.length, 0)
  let timer: ReturnType<typeof setTimeout> | undefined

  const close = () => {
    if (closed) return
    closed = true
    clearTimeout(timer)
    buffered = []
    socket.onopen = null
    socket.onmessage = null
    socket.onclose = null
    socket.onerror = null
    socket.close(1000)
  }
  const sendEarly = () => {
    socket.send(JSON.stringify({ ...taken.command, trace: { traceparent: taken.traceparent } }))
    subscribeSentAt = performance.now()
  }
  /** Give up the early subscription: the SDK's own subscribe then goes out normally. */
  const release = () => {
    if (state !== 'held') return
    state = 'released'
    clearTimeout(timer)
    buffered = buffered.filter((frame) => !frame.includes(earlyToken))
    if (socket.readyState === OPEN && subscribeSentAt !== undefined)
      socket.send(JSON.stringify({ kind: 'unsubscribe', id: taken.id }))
  }
  const overflowed = (frame: string) => {
    chars += frame.length
    return buffered.length >= maxFrames || chars > maxChars
  }

  // Until consumed: keep buffering, send the early subscribe if the socket opens after the take.
  socket.onopen = () => {
    if (subscribeSentAt === undefined) sendEarly()
  }
  socket.onmessage = (event) => {
    const frame = String(event.data)
    if (overflowed(frame)) {
      close()
      return
    }
    buffered.push(frame)
  }
  socket.onclose = close
  socket.onerror = close
  timer = setTimeout(close, consumeDeadlineMs)

  const deliver = (frame: string) => {
    if (!frame.includes(earlyToken)) {
      consumed?.onmessage?.({ data: frame })
      return
    }
    if (state === 'claimed' && sdkId !== undefined)
      consumed?.onmessage?.({ data: frame.replace(earlyToken, `"id":${JSON.stringify(sdkId)}`) })
    else if (state === 'held') {
      if (overflowed(frame)) release()
      else buffered.push(frame)
    }
  }

  const wouldAdopt = (command: Readonly<Record<string, unknown>>): EarlyAdoption | undefined =>
    consumed !== undefined &&
    !closed &&
    state === 'held' &&
    subscribeSentAt !== undefined &&
    command['kind'] === 'subscribe' &&
    subscriptionKey(command) === earlyKey
      ? { traceparent: taken.traceparent, subscribeSentAt }
      : undefined

  const consume = (): CollectionSocket | undefined => {
    if (consumed !== undefined || closed) {
      close()
      return undefined
    }
    clearTimeout(timer)
    const pending = buffered
    buffered = []
    chars = 0
    const adopted: CollectionSocket = {
      onopen: null,
      onmessage: null,
      onclose: null,
      onerror: null,
      send: (data) => {
        if (closed) return
        const command = JSON.parse(data) as Record<string, unknown>
        if (wouldAdopt(command) !== undefined && typeof command['id'] === 'string') {
          state = 'claimed'
          sdkId = command['id']
          clearTimeout(timer)
          const held = buffered
          buffered = []
          // After the client's synchronous send bookkeeping, before any later socket event.
          queueMicrotask(() => {
            for (const frame of held) deliver(frame)
          })
          return
        }
        if (state === 'claimed' && command['id'] === sdkId) {
          socket.send(JSON.stringify({ ...command, id: taken.id }))
          if (command['kind'] === 'unsubscribe') state = 'released'
          return
        }
        socket.send(data)
      },
      close,
    }
    consumed = adopted
    const opened = () => {
      if (closed) return
      if (subscribeSentAt === undefined) sendEarly()
      adopted.onopen?.()
      for (const frame of pending) deliver(frame)
      timer = setTimeout(release, claimDeadlineMs)
    }
    socket.onmessage = (event) => deliver(String(event.data))
    socket.onclose = (event) => {
      const handler = adopted.onclose
      close()
      handler?.(event)
    }
    socket.onerror = (event) => adopted.onerror?.(event)
    // The client assigns its handlers right after the factory returns.
    if (socket.readyState === OPEN) queueMicrotask(opened)
    else socket.onopen = opened
    return adopted
  }

  return { wouldAdopt, consume, close }
}
