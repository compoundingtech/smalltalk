import { it } from '@effect/vitest'
import { St3Client, type CollectionCommand, type CollectionSocket, type Snapshot } from '@smalltalk/st3-client'
import { Effect, SubscriptionRef } from 'effect'
import { afterEach, beforeEach, describe, expect, vi } from 'vitest'
import { makeChannel, type Channel } from './socket.ts'

const snapshot: Snapshot = {
  id: 'snapshot/example', created_at: '2026-10-03T00:00:00Z', host_id: 'host/example',
  projection_version: 'client-projection.v0', store_index: 1,
}

/** The real generated client talks only to this in-memory transport boundary. */
class Gateway {
  socket: CollectionSocket | undefined
  readonly commands: CollectionCommand[] = []
  reads = 0
  sockets = 0
  closes = 0
  unreachable = false
  autoOpen = true
  readonly fetch: typeof fetch = async () => {
    this.reads += 1
    if (this.unreachable) throw new TypeError('The gateway is unreachable')
    return Response.json({ api_version: 'st3.client.v0', snapshot, value: { capabilities: [] } })
  }
  readonly factory = () => {
    const socket: CollectionSocket = {
      onopen: null, onmessage: null, onclose: null, onerror: null,
      send: text => this.commands.push(JSON.parse(text)),
      close: () => { this.closes += 1 },
    }
    this.socket = socket
    this.sockets += 1
    if (this.autoOpen) queueMicrotask(() => socket.onopen?.())
    return socket
  }
  snapshot() {
    const command = this.commands.findLast(command => command.kind === 'subscribe')
    if (command === undefined) throw new Error('No subscription to answer')
    this.socket?.onmessage?.({ data: JSON.stringify({
      kind: 'snapshot', id: command.id, collection: 'agents', has_more: false,
      items: [], order: [], snapshot,
    }) })
  }
}

beforeEach(() => vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] }))
afterEach(() => { vi.restoreAllMocks(); vi.useRealTimers() })
const settle = Effect.promise(async () => {
  for (let round = 0; round < 10; round += 1) {
    await new Promise<void>(resolve => setImmediate(resolve))
    await vi.advanceTimersByTimeAsync(0)
  }
})
const withChannel = (test: (channel: Channel, gateway: Gateway) => Effect.Effect<void>) =>
  Effect.gen(function* () {
    const gateway = new Gateway()
    const channel = yield* makeChannel({
      client: new St3Client({ baseUrl: 'http://alpha.example', fetchImpl: gateway.fetch }), socket: gateway.factory,
    })
    yield* channel.register({ id: 'follow/example', subscriber: {
      probeEligible: true,
      subscribe: ({ stream, id }) => Effect.sync(() => stream.subscribe(id, 'agents', 1)),
      onFrame: () => channel.onDecodedFrame(), onDrop: () => {}, onRejected: () => {},
    } })
    yield* settle
    gateway.snapshot()
    yield* settle
    yield* test(channel, gateway)
  }).pipe(Effect.scoped)

describe('scoped connection recovery', () => {
  it.live('cancels a pending probe when its follow unregisters', () => withChannel((channel, gateway) => Effect.gen(function* () {
    yield* channel.probe
    yield* settle
    const reads = gateway.reads
    channel.unregister('follow/example')
    yield* Effect.promise(() => vi.advanceTimersByTimeAsync(15_000))
    yield* settle
    expect(gateway.closes).toBe(0)
    expect(gateway.reads).toBe(reads)
    expect((yield* SubscriptionRef.get(channel.connection))._tag).toBe('Live')
  })))

  it.live('cancels a superseded probe generation without closing the replacement', () => withChannel((channel, gateway) => Effect.gen(function* () {
    yield* channel.probe
    yield* settle
    const reads = gateway.reads
    yield* channel.resubscribe('follow/example')
    yield* settle
    gateway.snapshot()
    yield* settle
    yield* Effect.promise(() => vi.advanceTimersByTimeAsync(15_000))
    yield* settle
    expect(gateway.closes).toBe(0)
    expect(gateway.reads).toBe(reads)
    expect((yield* SubscriptionRef.get(channel.connection))._tag).toBe('Live')
  })))

  it.live('keeps visible-resume signals from bypassing scheduled backoff', () => withChannel((channel, gateway) => Effect.gen(function* () {
    gateway.unreachable = true
    gateway.socket?.onclose?.({ code: 1006, reason: '' })
    yield* settle
    const reads = gateway.reads
    for (let resume = 0; resume < 3; resume += 1) { yield* channel.probe; yield* settle }
    expect(gateway.reads).toBe(reads)
    yield* Effect.promise(() => vi.advanceTimersByTimeAsync(500))
    yield* settle
    expect(gateway.reads).toBe(reads + 1)
    for (let resume = 0; resume < 3; resume += 1) { yield* channel.probe; yield* settle }
    expect(gateway.reads).toBe(reads + 1)
  })))

  it.live('coalesces manual actions while a socket upgrade is pending', () => withChannel((channel, gateway) => Effect.gen(function* () {
    gateway.autoOpen = false
    gateway.socket?.onclose?.({ code: 1006, reason: '' })
    yield* settle
    const reads = gateway.reads
    const sockets = gateway.sockets
    yield* channel.reconnect
    yield* settle
    for (let press = 0; press < 3; press += 1) { yield* channel.reconnect; yield* settle }
    expect(gateway.reads).toBe(reads + 1)
    expect(gateway.sockets).toBe(sockets + 1)
    gateway.socket?.onopen?.()
    yield* settle
    gateway.snapshot()
    yield* settle
    expect((yield* SubscriptionRef.get(channel.connection))._tag).toBe('Live')
  })))

  it.live('does not log raw id-less server error text or codes', () => withChannel((channel, gateway) => Effect.gen(function* () {
    const warning = vi.spyOn(console, 'warn').mockImplementation(() => {})
    gateway.socket?.onmessage?.({ data: JSON.stringify({ kind: 'error', code: 'unrendered-code', message: 'raw-server-error-sentinel' }) })
    yield* settle
    expect(warning).not.toHaveBeenCalled()
  })))
})
