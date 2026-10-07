/**
 * Follow admission through the real SDK against a scripted collections socket: which follows
 * keep their slot, which one is evicted, and what each consumer's stream sees.
 */
import type { CollectionFrame, CollectionSocket } from '@smalltalk/st3-client'
import type { CollectionName, Snapshot } from '@smalltalk/st3-client'
import * as Effect from 'effect/Effect'
import * as Fiber from 'effect/Fiber'
import * as Stream from 'effect/Stream'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { type FollowEvent, type FollowSpec, followKey, St3, St3Live, type WindowValue } from './mod.ts'

const API_VERSION = 'st3.client.v0'
const snapshot: Snapshot = {
  id: 'snapshot/1',
  created_at: '2026-10-03T00:00:00Z',
  host_id: 'host/build-a',
  projection_version: 'client-projection.v0',
  store_index: 1,
}

/** The gateway's side of one collections socket: the commands it got and a way to answer. */
class FakeGateway {
  socket: (CollectionSocket & { opened: boolean }) | undefined
  closeCalls = 0
  autoOpen = true
  closeImmediatelyAfterOpen = false
  readonly commands: { kind: string; id: string; collection?: string }[] = []
  readonly sentSubscribeIds: string[] = []
  readonly sentFollowSubscribes: Array<{ readonly id: string; readonly key: string }> = []

  readRuntime: (() => Promise<unknown>) | undefined
  attach: (() => Promise<unknown>) | undefined
  readonly fetch: typeof fetch = async (input, init) => {
    const url = String(input)
    const value =
      url.includes('/runtimes/') && this.readRuntime !== undefined
        ? await this.readRuntime()
        : init?.method === 'POST' && this.attach !== undefined
          ? await this.attach()
          : {}
    return new Response(JSON.stringify({ api_version: API_VERSION, snapshot, value }), {
      status: 200,
      headers: { 'content-type': 'application/json' },
    })
  }

  readonly factory = () => {
    const socket: CollectionSocket & { opened: boolean } = {
      opened: false,
      onopen: null,
      onmessage: null,
      onclose: null,
      onerror: null,
      send: (text: string) => {
        this.commands.push(JSON.parse(text))
      },
      close: () => { this.closeCalls += 1 },
    }
    this.socket = socket
    if (this.autoOpen) queueMicrotask(() => this.open())
    if (this.closeImmediatelyAfterOpen) queueMicrotask(() => { this.open(); this.end() })
    return socket
  }

  open() {
    if (!this.socket) throw new Error('no socket to open')
    this.socket.opened = true
    this.socket.onopen?.()
  }

  end() {
    this.socket?.onclose?.({ code: 1006, reason: 'closed before SDK resumed' })
  }

  /** The subscription id the SDK used for `collection` (the most recent subscribe). */
  idOf(collection: CollectionName): string {
    const command = this.commands.findLast(
      (entry) => entry.kind === 'subscribe' && entry.collection === collection,
    )
    if (command === undefined) throw new Error(`no subscribe for ${collection}`)
    return command.id
  }

  subscribes(collection: CollectionName): number {
    return this.commands.filter(
      (entry) => entry.kind === 'subscribe' && entry.collection === collection,
    ).length
  }

  send(frame: CollectionFrame) {
    this.socket?.onmessage?.({ data: JSON.stringify(frame) })
  }

  window(collection: CollectionName, hasMore: boolean) {
    this.send({
      kind: 'snapshot',
      id: this.idOf(collection),
      collection,
      has_more: hasMore,
      items: [],
      order: [],
      snapshot,
    })
  }
}

const windowSpec = (collection: CollectionName): Extract<FollowSpec, { _tag: 'Window' }> => ({
  _tag: 'Window',
  collection,
  limit: 50,
})

/** Drain pending socket callbacks, stream pulls and forked fibers (no wall-clock wait). */
const settle = Effect.promise(async () => {
  for (let round = 0; round < 10; round += 1) {
    await new Promise<void>((resolve) => setImmediate(resolve))
    await vi.advanceTimersByTimeAsync(0)
  }
})

/** One mounted consumer of a follow: everything it saw and whether its stream ended. */
interface Consumer {
  readonly events: FollowEvent<WindowValue>[]
  readonly ended: () => boolean
  readonly fiber: Fiber.Fiber<void>
}

const mount = (spec: Extract<FollowSpec, { _tag: 'Window' }>) =>
  Effect.gen(function* () {
    const st3 = yield* St3
    const events: FollowEvent<WindowValue>[] = []
    let ended = false
    const fiber = yield* Effect.forkChild(
      st3.followWindow(spec).pipe(
        Stream.runForEach((event) => Effect.sync(() => events.push(event))),
        Effect.ensuring(Effect.sync(() => (ended = true))),
      ),
    )
    yield* settle
    return { events, ended: () => ended, fiber } satisfies Consumer
  })

const run = (
  maxFollows: number,
  test: (gateway: FakeGateway) => Effect.Effect<void, never, St3>,
  autoOpen = true,
) => {
  const gateway = new FakeGateway()
  gateway.autoOpen = autoOpen
  const layer = St3Live({
    baseUrl: 'http://gateway.test',
    maxFollows,
    socket: gateway.factory,
    fetch: gateway.fetch,
    onSubscribeSent: (id) => gateway.sentSubscribeIds.push(id),
    onFollowSubscribeSent: (event) => gateway.sentFollowSubscribes.push(event),
  })
  return Effect.gen(function* () {
    yield* settle
    yield* test(gateway)
  }).pipe(Effect.provide(layer), Effect.scoped, Effect.runPromise)
}

const tags = (consumer: Consumer) => consumer.events.map((event) => event._tag)

describe('follow admission', () => {
  beforeEach(() => vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] }))
  afterEach(() => vi.useRealTimers())

  it('correlates actual subscribe sends through queued open and reconnect', () =>
    run(1, (gateway) =>
      Effect.gen(function* () {
        const spec = windowSpec('agents')
        const consumer = yield* mount(spec)
        expect(gateway.commands).toEqual([])
        expect(gateway.sentSubscribeIds).toEqual([])
        expect(gateway.sentFollowSubscribes).toEqual([])

        gateway.open()
        yield* settle
        const firstId = gateway.idOf('agents')
        expect(gateway.sentSubscribeIds).toEqual([firstId])
        expect(gateway.sentFollowSubscribes).toEqual([{ id: firstId, key: followKey(spec) }])

        gateway.autoOpen = true
        gateway.end()
        yield* settle
        yield* Effect.promise(() => vi.advanceTimersByTimeAsync(500))
        yield* settle
        const subscribeIds = gateway.sentSubscribeIds
        expect(subscribeIds).toHaveLength(2)
        expect(gateway.sentFollowSubscribes).toEqual([
          { id: firstId, key: followKey(spec) },
          { id: firstId, key: followKey(spec) },
        ])

        yield* Fiber.interrupt(consumer.fiber)
        yield* settle
        expect(gateway.sentFollowSubscribes).toHaveLength(2)
      }),
      false,
    ))

  it('reports the shared connection Live only after the socket open event', () =>
    run(1, (gateway) =>
      Effect.gen(function* () {
        const st3 = yield* St3
        const states: string[] = []
        const fiber = yield* Effect.forkChild(
          st3.connection.pipe(Stream.runForEach((state) => Effect.sync(() => states.push(state._tag)))),
        )
        yield* settle
        expect(states).toContain('Connecting')
        expect(states).not.toContain('Live')
        gateway.open()
        yield* settle
        expect(states).toContain('Live')
        yield* Fiber.interrupt(fiber)
      }),
      false,
    ))

  it('does not report Live when open and end arrive before the channel resumes', () => {
    const gateway = new FakeGateway()
    gateway.autoOpen = false
    gateway.closeImmediatelyAfterOpen = true
    const layer = St3Live({
      baseUrl: 'http://gateway.test',
      maxFollows: 1,
      socket: gateway.factory,
      fetch: gateway.fetch,
    })
    const states: string[] = []
    return Effect.runPromise(
      Effect.gen(function* () {
        const st3 = yield* St3
        yield* Effect.forkChild(
          st3.connection.pipe(Stream.runForEach((state) => Effect.sync(() => states.push(state._tag)))),
        )
        yield* settle
        expect(states).not.toContain('Live')
      }).pipe(Effect.provide(layer), Effect.scoped),
    )
  })

  it('closes an acquired socket exactly once when the scope ends before open', async () => {
    const gateway = new FakeGateway()
    gateway.autoOpen = false
    const layer = St3Live({
      baseUrl: 'http://gateway.test',
      maxFollows: 1,
      socket: gateway.factory,
      fetch: gateway.fetch,
    })
    const states: string[] = []

    await Effect.runPromise(
      Effect.gen(function* () {
        const st3 = yield* St3
        yield* Effect.forkChild(
          st3.connection.pipe(Stream.runForEach((state) => Effect.sync(() => states.push(state._tag)))),
        )
        yield* settle
        expect(gateway.socket).toBeDefined()
        expect(states).toContain('Connecting')
        expect(states).not.toContain('Live')
      }).pipe(Effect.provide(layer), Effect.scoped),
    )

    expect(gateway.closeCalls).toBe(1)
    expect(gateway.socket?.onopen).toBeNull()
    expect(gateway.socket?.onmessage).toBeNull()
    expect(gateway.socket?.onclose).toBeNull()
    expect(gateway.socket?.onerror).toBeNull()
    gateway.open()
    await Promise.resolve()
    expect(gateway.closeCalls).toBe(1)
    expect(gateway.commands).toEqual([])
    expect(states).not.toContain('Live')
  })

  it('marks resync stale and subscribes again before accepting an authoritative window', () =>
    run(1, (gateway) =>
      Effect.gen(function* () {
        const agents = yield* mount(windowSpec('agents'))
        const followId = gateway.idOf('agents')
        expect(gateway.sentSubscribeIds).toEqual([followId])
        gateway.window('agents', true)
        yield* settle
        gateway.send({ kind: 'resync', id: followId, code: 'internal', message: 'Retry the read', retryable: true })
        yield* settle
        expect(tags(agents)).toEqual(['Observed', 'Stale'])
        expect(agents.events.at(-1)).toEqual({ _tag: 'Stale', code: 'internal', message: 'Retry the read' })
        expect(gateway.subscribes('agents')).toBe(1)
        yield* Effect.promise(() => vi.advanceTimersByTimeAsync(1000))
        yield* settle
        expect(gateway.subscribes('agents')).toBe(2)
        expect(gateway.sentSubscribeIds).toEqual([followId, followId])
        gateway.window('agents', false)
        yield* settle
        expect(agents.events.at(-1)).toEqual({
          _tag: 'Observed',
          value: { items: [], hasMore: false, rawItems: [], snapshot },
        })
        expect(agents.ended()).toBe(false)
      }),
    ))

  it('coalesces repeated resyncs and paces subscriptions during a sustained failure', () =>
    run(1, (gateway) =>
      Effect.gen(function* () {
        const agents = yield* mount(windowSpec('agents'))
        gateway.window('agents', true)
        yield* settle
        const resync = () =>
          gateway.send({ kind: 'resync', id: gateway.idOf('agents'), code: 'internal', message: 'Retry the read', retryable: true })
        for (let attempt = 0; attempt < 5; attempt += 1) resync()
        yield* settle
        expect(gateway.subscribes('agents')).toBe(1)
        expect(agents.events.at(-1)).toMatchObject({ _tag: 'Stale', code: 'internal', message: 'Retry the read' })
        yield* Effect.promise(() => vi.advanceTimersByTimeAsync(999))
        expect(gateway.subscribes('agents')).toBe(1)
        yield* Effect.promise(() => vi.advanceTimersByTimeAsync(1))
        yield* settle
        expect(gateway.subscribes('agents')).toBe(2)

        // A sustained outage cannot bypass the next interval with another burst of resyncs.
        for (let attempt = 0; attempt < 5; attempt += 1) resync()
        yield* settle
        yield* Effect.promise(() => vi.advanceTimersByTimeAsync(999))
        expect(gateway.subscribes('agents')).toBe(2)
        yield* Effect.promise(() => vi.advanceTimersByTimeAsync(1))
        yield* settle
        expect(gateway.subscribes('agents')).toBe(3)
        gateway.window('agents', false)
        yield* settle
        expect(agents.events.at(-1)).toEqual({
          _tag: 'Observed',
          value: { items: [], hasMore: false, rawItems: [], snapshot },
        })
        yield* Effect.promise(() => vi.advanceTimersByTimeAsync(5000))
        expect(gateway.subscribes('agents')).toBe(3)
        expect(agents.ended()).toBe(false)
      }),
    ))

  it.each(['runtime read', 'terminal attach'])(
    'does not subscribe after eviction during %s',
    (phase) =>
      run(1, (gateway) =>
        Effect.gen(function* () {
          const runtime = {
            kind: 'runtime',
            id: 'runtime/a',
            revision: '1',
            desired_revision: null,
            incarnation_id: 'incarnation/a',
            owner_host_id: 'host/test',
            owner_id: 'agent/a',
            runtime_id: 'a',
            runtime_kind: 'agent',
            state: 'running',
            terminal_id: 'terminal/a',
            updated_at: '2026-10-03T00:00:00Z',
          }
          const attachment = {
            terminal_attachment: {
              runtime_incarnation: 'incarnation/a',
              stream_capability: 'capability/a',
            },
          }
          let release: (() => void) | undefined
          let entered = false
          const pending = new Promise<unknown>((resolve) => {
            release = () => resolve(phase === 'runtime read' ? runtime : attachment)
          })
          gateway.readRuntime = () => {
            if (phase !== 'runtime read') return Promise.resolve(runtime)
            entered = true
            return pending
          }
          gateway.attach = () => {
            if (phase !== 'terminal attach') return Promise.resolve(attachment)
            entered = true
            return pending
          }
          const st3 = yield* St3
          const spec = { _tag: 'Terminal', runtime: 'runtime/a' } as const
          const events: string[] = []
          let ended = false
          yield* Effect.forkChild(
            st3.followTerminal(spec).pipe(
              Stream.runForEach((event) => Effect.sync(() => events.push(event._tag))),
              Effect.ensuring(Effect.sync(() => (ended = true))),
            ),
          )
          yield* settle
          expect(entered).toBe(true)
          yield* st3.setVisible(spec, false)
          yield* mount(windowSpec('missions'))
          release?.()
          yield* settle
          expect(events).toEqual(['Stale'])
          expect(ended).toBe(true)
          expect(gateway.commands.filter((command) => command.collection === 'terminal')).toEqual(
            [],
          )
          expect(gateway.commands.filter((command) => command.kind === 'unsubscribe')).toHaveLength(
            1,
          )
          expect(gateway.subscribes('missions')).toBe(1)
        }),
      ),
  )

  it('evicts the least-recently-visible invisible follow and never a visible one', () =>
    run(3, (gateway) =>
      Effect.gen(function* () {
        const st3 = yield* St3
        const older = yield* mount(windowSpec('agents'))
        const visible = yield* mount(windowSpec('missions'))
        const newer = yield* mount(windowSpec('attention'))
        // `missions` was hidden first but is visible again; `agents` hid before `attention`.
        yield* st3.setVisible(windowSpec('missions'), false)
        yield* st3.setVisible(windowSpec('agents'), false)
        yield* st3.setVisible(windowSpec('attention'), false)
        yield* st3.setVisible(windowSpec('missions'), true)

        yield* mount(windowSpec('work'))

        expect(older.ended()).toBe(true)
        expect(tags(older).at(-1)).toBe('Stale')
        expect(visible.ended()).toBe(false)
        expect(newer.ended()).toBe(false)
        expect(gateway.commands).toContainEqual({ kind: 'unsubscribe', id: gateway.idOf('agents') })
      }),
    ))

  it('keeps folding an invisible follow and shows it again without resubscribing', () =>
    run(2, (gateway) =>
      Effect.gen(function* () {
        const st3 = yield* St3
        const agents = yield* mount(windowSpec('agents'))
        gateway.window('agents', false)
        yield* st3.setVisible(windowSpec('agents'), false)
        gateway.window('agents', true)
        yield* settle

        expect(agents.events).toEqual([
          { _tag: 'Observed', value: { items: [], hasMore: false, rawItems: [], snapshot } },
          { _tag: 'Observed', value: { items: [], hasMore: true, rawItems: [], snapshot } },
        ])

        yield* st3.setVisible(windowSpec('agents'), true)
        yield* settle
        expect(gateway.subscribes('agents')).toBe(1)
        expect(agents.ended()).toBe(false)
      }),
    ))

  it('ends an evicted follow with Stale; opening the same spec again resubscribes', () =>
    run(1, (gateway) =>
      Effect.gen(function* () {
        const st3 = yield* St3
        const agents = yield* mount(windowSpec('agents'))
        yield* st3.setVisible(windowSpec('agents'), false)
        const missions = yield* mount(windowSpec('missions'))

        expect(tags(agents)).toEqual(['Stale'])
        expect(agents.ended()).toBe(true)

        yield* st3.setVisible(windowSpec('missions'), false)
        yield* Fiber.interrupt(missions.fiber)
        const again = yield* mount(windowSpec('agents'))
        expect(gateway.subscribes('agents')).toBe(2)
        gateway.window('agents', false)
        yield* settle
        expect(tags(again)).toEqual(['Observed'])
      }),
    ))

  it('lowers the cap on a server subscription limit and evicts to make room', () =>
    run(8, (gateway) =>
      Effect.gen(function* () {
        const st3 = yield* St3
        const agents = yield* mount(windowSpec('agents'))
        yield* st3.setVisible(windowSpec('agents'), false)
        yield* mount(windowSpec('missions'))
        const attention = yield* mount(windowSpec('attention'))

        // Today's daemon: an error without a code names the refused subscription.
        gateway.send({
          kind: 'error',
          id: gateway.idOf('attention'),
          message: 'invalid subscription or subscription limit exceeded',
        })
        yield* settle

        expect(tags(agents)).toEqual(['Stale'])
        expect(agents.ended()).toBe(true)
        expect(gateway.subscribes('attention')).toBe(2)
        expect(attention.ended()).toBe(false)

        // The cap is now 2: a fourth follow with every held one visible is refused.
        const work = yield* mount(windowSpec('work'))
        expect(tags(work)).toEqual(['Failed'])
        expect(work.ended()).toBe(true)
      }),
    ))

  it('refuses a new follow at the cap when every held follow is visible', () =>
    run(1, (gateway) =>
      Effect.gen(function* () {
        const agents = yield* mount(windowSpec('agents'))
        const missions = yield* mount(windowSpec('missions'))

        expect(missions.events).toMatchObject([
          { _tag: 'Failed', error: { _tag: 'SubscriptionLimit', cap: 1 } },
        ])
        expect(missions.ended()).toBe(true)
        expect(agents.ended()).toBe(false)
        expect(gateway.subscribes('missions')).toBe(0)
      }),
    ))
})
