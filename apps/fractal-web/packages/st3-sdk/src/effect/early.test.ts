/**
 * Early-connect adoption: the SDK takes the socket `index.html` opened, answers its own roster
 * subscribe from the early subscription (no second subscribe, no second socket), replays the
 * buffered snapshot under its own id, and keeps every holding state bounded.
 */
import type { CollectionFrame, CollectionSocket, Snapshot } from '@smalltalk/st3-client'
import { St3Client } from '@smalltalk/st3-client'
import * as Effect from 'effect/Effect'
import * as Exit from 'effect/Exit'
import * as Stream from 'effect/Stream'
import * as Tracer from 'effect/Tracer'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { EARLY_COLLECTIONS_GLOBAL, peekEarlyBootstrap, takeEarlyCollections } from './early.ts'
import { FIRST_FRAME_SPAN, type FollowEvent, St3, St3Live, type WindowValue } from './mod.ts'

const API_VERSION = 'st3.client.v0'
const EARLY_ID = 'wf-early-0123456789abcdef'
const BOOTSTRAP = '00-0123456789abcdef0123456789abcdef-0123456789abcdef-01'
const snapshot: Snapshot = {
  id: 'snapshot/1',
  created_at: '2026-10-08T00:00:00Z',
  host_id: 'host/build-a',
  projection_version: 'client-projection.v0',
  store_index: 1,
}
const rosterSnapshot = (id: string): CollectionFrame => ({
  kind: 'snapshot',
  id,
  collection: 'agents',
  has_more: false,
  items: [],
  order: [],
  snapshot,
})

/** The browser socket the inline script opened, as the SDK sees it after the take. */
class RawSocket {
  readyState = 0
  onopen: ((event: unknown) => void) | null = null
  onmessage: ((event: { readonly data: unknown }) => void) | null = null
  onclose: ((event: { readonly code: number; readonly reason: string }) => void) | null = null
  onerror: ((event: unknown) => void) | null = null
  readonly sent: Record<string, unknown>[] = []
  closed = 0
  send(data: string) {
    this.sent.push(JSON.parse(data))
  }
  close() {
    this.closed += 1
    this.readyState = 3
  }
  open() {
    this.readyState = 1
    this.onopen?.({})
  }
  receive(frame: CollectionFrame) {
    // Serde emits compact JSON; JSON.stringify matches that spelling.
    this.onmessage?.({ data: JSON.stringify(frame) })
  }
}

/** A published inline-script handle whose subscribe already went out. */
const publish = ({ frames = [rosterSnapshot(EARLY_ID)], open = true }: { frames?: CollectionFrame[]; open?: boolean } = {}) => {
  const socket = new RawSocket()
  if (open) socket.readyState = 1
  const target: Record<string, unknown> = {}
  let taken = false
  target[EARLY_COLLECTIONS_GLOBAL] = {
    id: EARLY_ID,
    traceparent: BOOTSTRAP,
    startedAt: 1,
    take: () => {
      if (taken) return undefined
      taken = true
      delete target[EARLY_COLLECTIONS_GLOBAL]
      return {
        socket,
        id: EARLY_ID,
        command: { kind: 'subscribe', id: EARLY_ID, collection: 'agents', limit: 100 },
        traceparent: BOOTSTRAP,
        startedAt: 1,
        ...(open ? { subscribeSentAt: 2 } : {}),
        frames: frames.map((frame) => JSON.stringify(frame)),
      }
    },
  }
  return { socket, target }
}

/** Drain socket callbacks, microtask replays and zero-delay timers (no wall-clock wait). */
const settle = async () => {
  for (let round = 0; round < 10; round += 1) {
    await new Promise<void>((resolve) => setImmediate(resolve))
    await vi.advanceTimersByTimeAsync(0)
  }
}

beforeEach(() => vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] }))
afterEach(() => vi.useRealTimers())

const noHttp = new St3Client({ baseUrl: 'http://gateway.test', fetchImpl: async () => { throw new Error('no HTTP') } })

describe('takeEarlyCollections', () => {
  it('peeks the bootstrap context without taking the socket', () => {
    const { target } = publish()
    expect(peekEarlyBootstrap(target)).toEqual({ traceparent: BOOTSTRAP, startedAt: 1 })
    expect(target[EARLY_COLLECTIONS_GLOBAL]).toBeDefined()
  })

  it('answers the identical subscribe from the early subscription and replays its snapshot', async () => {
    const { socket, target } = publish()
    const early = takeEarlyCollections(target)!
    expect(target[EARLY_COLLECTIONS_GLOBAL]).toBeUndefined()
    const frames: CollectionFrame[] = []
    const sent: string[] = []
    const stream = await noHttp.collectionStream({
      onFrame: (frame) => frames.push(frame),
      onCommandSent: (command) => sent.push(`${command.kind}:${command.id}`),
      socket: () => early.consume()!,
    })
    await settle()
    expect(early.wouldAdopt({ kind: 'subscribe', id: 'f1', collection: 'agents', limit: 100 })).toEqual({ traceparent: BOOTSTRAP, subscribeSentAt: 2 })
    stream.subscribe('f1', 'agents', 100)
    // The client believes it sent; the socket saw no second subscribe.
    expect(sent).toEqual(['subscribe:f1'])
    expect(socket.sent).toEqual([])
    await settle()
    expect(frames).toEqual([rosterSnapshot('f1')])
    socket.receive({ ...rosterSnapshot(EARLY_ID), kind: 'changes', upserts: [], removes: [] } as CollectionFrame)
    expect(frames[1]).toMatchObject({ kind: 'changes', id: 'f1' })
    stream.unsubscribe('f1')
    expect(socket.sent).toEqual([{ kind: 'unsubscribe', id: EARLY_ID }])
    // A later roster follow subscribes normally on the adopted socket.
    stream.subscribe('f2', 'agents', 100)
    expect(socket.sent[1]).toMatchObject({ kind: 'subscribe', id: 'f2', collection: 'agents' })
    stream.close()
    expect(socket.closed).toBe(1)
  })

  it('sends the early subscribe itself when the socket opens after the take', async () => {
    const { socket, target } = publish({ frames: [], open: false })
    const early = takeEarlyCollections(target)!
    socket.open()
    expect(socket.sent).toEqual([{ kind: 'subscribe', id: EARLY_ID, collection: 'agents', limit: 100, trace: { traceparent: BOOTSTRAP } }])
    early.close()
    expect(socket.closed).toBe(1)
  })

  it('refuses a socket that ended before the SDK consumed it', () => {
    const { socket, target } = publish()
    const early = takeEarlyCollections(target)!
    socket.onclose?.({ code: 1006, reason: '' })
    expect(early.consume()).toBeUndefined()
    expect(socket.closed).toBe(1)
  })

  it('closes on buffer overflow and after the consume deadline', async () => {
    const overflow = publish({ frames: [] })
    const overflowing = takeEarlyCollections(overflow.target, { maxFrames: 2 })!
    for (let frame = 0; frame < 3; frame += 1) overflow.socket.receive(rosterSnapshot(EARLY_ID))
    expect(overflow.socket.closed).toBe(1)
    expect(overflowing.consume()).toBeUndefined()

    const idle = publish()
    takeEarlyCollections(idle.target, { consumeDeadlineMs: 1_000 })
    await vi.advanceTimersByTimeAsync(999)
    expect(idle.socket.closed).toBe(0)
    await vi.advanceTimersByTimeAsync(1)
    expect(idle.socket.closed).toBe(1)
  })

  it('unsubscribes an early subscription nobody claims, then passes subscribes through', async () => {
    const { socket, target } = publish()
    const early = takeEarlyCollections(target, { claimDeadlineMs: 1_000 })!
    const frames: CollectionFrame[] = []
    const stream = await noHttp.collectionStream({ onFrame: (frame) => frames.push(frame), socket: () => early.consume()! })
    await settle()
    expect(socket.sent).toEqual([])
    await vi.advanceTimersByTimeAsync(1_000)
    expect(socket.sent).toEqual([{ kind: 'unsubscribe', id: EARLY_ID }])
    stream.subscribe('f1', 'agents', 100)
    expect(socket.sent[1]).toMatchObject({ kind: 'subscribe', id: 'f1' })
    expect(frames).toEqual([])
    stream.close()
  })

  it('closes a handle whose data it cannot verify', () => {
    const { socket, target } = publish()
    const handle = target[EARLY_COLLECTIONS_GLOBAL] as { take: () => Record<string, unknown> }
    const take = handle.take
    handle.take = () => ({ ...take(), traceparent: 'not-a-traceparent' })
    expect(takeEarlyCollections(target)).toBeUndefined()
    expect(socket.closed).toBe(1)
  })
})

describe('St3Live adopting the early socket', () => {
  it('serves the roster from the early subscription: one socket, no duplicate subscribe, early first-frame root', async () => {
    const { socket, target } = publish()
    const previous = Reflect.get(globalThis, EARLY_COLLECTIONS_GLOBAL)
    Reflect.set(globalThis, EARLY_COLLECTIONS_GLOBAL, target[EARLY_COLLECTIONS_GLOBAL])
    const spans: Tracer.NativeSpan[] = []
    const tracer = Tracer.make({
      span: (options) => {
        const span = new Tracer.NativeSpan(options)
        spans.push(span)
        return span
      },
    })
    let freshSockets = 0
    const events: FollowEvent<WindowValue>[] = []
    try {
      await Effect.gen(function* () {
        const st3 = yield* St3
        yield* Effect.forkChild(
          st3.followWindow({ _tag: 'Window', collection: 'agents', limit: 100 }).pipe(
            Stream.runForEach((event) => Effect.sync(() => events.push(event))),
          ),
        )
        yield* Effect.promise(settle)
      }).pipe(
        Effect.provide(
          St3Live({
            baseUrl: 'http://gateway.test',
            maxFollows: 4,
            adoptEarlyCollections: true,
            fetch: async () => new Response(JSON.stringify({ api_version: API_VERSION, snapshot, value: {} })),
            socket: () => {
              freshSockets += 1
              return { onopen: null, onmessage: null, onclose: null, onerror: null, send() {}, close() {} } satisfies CollectionSocket
            },
          }),
        ),
        Effect.scoped,
        Effect.withTracer(tracer),
        Effect.runPromise,
      )
    } finally {
      Reflect.set(globalThis, EARLY_COLLECTIONS_GLOBAL, previous)
    }
    expect(freshSockets).toBe(0)
    expect(socket.sent).toEqual([])
    expect(events.map((event) => event._tag)).toContain('Observed')
    const root = spans.find((span) => span.name === FIRST_FRAME_SPAN)!
    expect(root.parent._tag).toBe('None')
    expect(root.links[0]?.span.traceId).toBe(BOOTSTRAP.slice(3, 35))
    expect(root.links[0]?.span.spanId).toBe(BOOTSTRAP.slice(36, 52))
    expect(root.attributes.get('wf.ux.early')).toBe(true)
    expect(root.attributes.get('span.label')).toBe('agents')
    expect(root.attributes.get('wf.ux.outcome')).toBe('observed')
    expect(root.status._tag === 'Ended' && Exit.isSuccess(root.status.exit)).toBe(true)
    // Layer release closed the adopted socket.
    expect(socket.closed).toBe(1)
  })
})
