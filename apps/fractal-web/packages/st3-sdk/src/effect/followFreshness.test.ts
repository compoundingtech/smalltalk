/**
 * Per-follow freshness through the real SDK against a scripted socket: Requested only after a
 * real send, Live only after a real frame, Reconnecting while the socket is down (never Live),
 * Stale(Evicted) when the conversation pool reclaims a slot, and no leaked table entries.
 */
import { it as effectIt } from '@effect/vitest'
import type { CollectionFrame, CollectionSocket } from '@smalltalk/st3-client'
import type { CollectionName, Snapshot } from '@smalltalk/st3-client'
import * as Effect from 'effect/Effect'
import * as Fiber from 'effect/Fiber'
import type * as Scope from 'effect/Scope'
import * as Stream from 'effect/Stream'
import * as SubscriptionRef from 'effect/SubscriptionRef'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import {
  type ConversationChunk,
  type FollowEvent,
  followKey,
  St3,
  type SyncStatus,
  St3Live,
  type WindowValue,
} from './mod.ts'
import type { FollowFreshness } from './freshness.ts'
import type { St3Diagnostic } from './socket.ts'

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
  autoOpen = true
  readonly commands: { kind: string; id: string; collection?: string; conversation?: string }[] = []

  collectionsV1 = false
  probe: (() => Promise<Response>) | undefined
  readonly fetch: typeof fetch = async () => {
    if (this.probe !== undefined) return this.probe()
    return new Response(
      JSON.stringify({
        api_version: 'st3.client.v0',
        snapshot,
        value: {
          capabilities: this.collectionsV1
            ? [{ id: 'collections', state: 'granted', version: 1 }]
            : [],
        },
      }),
      { status: 200, headers: { 'content-type': 'application/json' } },
    )
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
      close: () => {},
    }
    this.socket = socket
    if (this.autoOpen) queueMicrotask(() => this.open())
    return socket
  }

  open() {
    if (!this.socket) throw new Error('no socket to open')
    this.socket.opened = true
    this.socket.onopen?.()
  }

  end() {
    this.socket?.onclose?.({ code: 1006, reason: 'socket dropped' })
  }

  idOf(collection: CollectionName): string {
    const command = this.commands.findLast(
      (candidate) => candidate.kind === 'subscribe' && candidate.collection === collection,
    )
    if (command === undefined) throw new Error(`no subscribe for ${collection}`)
    return command.id
  }

  conversationId(ref: string): string {
    const command = this.commands.findLast(
      (candidate) => candidate.kind === 'subscribe' && candidate.conversation === ref,
    )
    if (command === undefined) throw new Error(`no subscribe for ${ref}`)
    return command.id
  }

  unsubscribed(): string[] {
    return this.commands.filter((command) => command.kind === 'unsubscribe').map((command) => command.id)
  }

  send(frame: CollectionFrame) {
    this.socket?.onmessage?.({ data: JSON.stringify(frame) })
  }

  window(collection: CollectionName) {
    this.send({
      kind: 'snapshot',
      id: this.idOf(collection),
      collection,
      has_more: false,
      items: [],
      order: [],
      snapshot,
    })
  }
}

/** Drain pending socket callbacks, stream pulls and forked fibers (no wall-clock wait). */
const settle = Effect.promise(async () => {
  for (let round = 0; round < 10; round += 1) {
    await new Promise<void>((resolve) => setImmediate(resolve))
    await vi.advanceTimersByTimeAsync(0)
  }
})

interface Mounted<A> {
  readonly events: FollowEvent<A>[]
  readonly freshness: FollowFreshness[]
  readonly statuses: SyncStatus[]
  readonly lastStatus: () => SyncStatus | undefined
  readonly tags: () => string[]
  readonly lastFreshness: () => FollowFreshness | undefined
  readonly ended: () => boolean
  readonly interrupt: Effect.Effect<void>
}

/** Mount one follow plus its freshness watcher; `interrupt` ends both. */
const watchFollow = <A>(
  st3: St3['Service'],
  key: string,
  follow: Stream.Stream<FollowEvent<A>>,
): Effect.Effect<Mounted<A>, never, Scope.Scope> =>
  Effect.gen(function* () {
    const events: FollowEvent<A>[] = []
    const freshness: FollowFreshness[] = []
    const statuses: SyncStatus[] = []
    let ended = false
    const fiber = yield* Effect.forkChild(
      follow.pipe(
        Stream.runForEach((event) => Effect.sync(() => events.push(event))),
        Effect.ensuring(Effect.sync(() => (ended = true))),
      ),
    )
    const watch = yield* Effect.forkChild(
      st3.followFreshness(key).pipe(
        Stream.runForEach((value) => Effect.sync(() => freshness.push(value))),
      ),
    )
    const statusWatch = yield* Effect.forkChild(
      st3.followSyncStatus(key).pipe(
        Stream.runForEach((value) => Effect.sync(() => statuses.push(value))),
      ),
    )
    yield* settle
    return {
      events,
      freshness,
      statuses,
      lastStatus: () => statuses.at(-1),
      tags: () => freshness.map((value) => value._tag),
      lastFreshness: () => freshness.at(-1),
      ended: () => ended,
      interrupt: Fiber.interrupt(fiber).pipe(
        Effect.andThen(Fiber.interrupt(watch)),
        Effect.andThen(Fiber.interrupt(statusWatch)),
      ),
    }
  })

const mountWindow = (
  collection: CollectionName,
): Effect.Effect<Mounted<WindowValue>, never, St3 | Scope.Scope> =>
  Effect.gen(function* () {
    const st3 = yield* St3
    return yield* watchFollow(
      st3,
      followKey({ _tag: 'Window', collection, limit: 50 }),
      st3.followWindow({ _tag: 'Window', collection, limit: 50 }),
    )
  })

const mountConversation = (
  ref: string,
): Effect.Effect<Mounted<ConversationChunk>, never, St3 | Scope.Scope> =>
  Effect.gen(function* () {
    const st3 = yield* St3
    return yield* watchFollow(
      st3,
      followKey({ _tag: 'Conversation', ref }),
      st3.followConversation({ _tag: 'Conversation', ref }),
    )
  })

const windowSpec = (collection: CollectionName) =>
  ({ _tag: 'Window', collection, limit: 50 }) as const

const runEffect = (
  options: {
    readonly maxFollows: number
    readonly conversationSlots?: number | 'advertised'
    readonly setup?: (gateway: FakeGateway) => void
    readonly onDiagnostics?: (event: St3Diagnostic) => void
  },
  test: (gateway: FakeGateway) => Effect.Effect<void, never, St3 | Scope.Scope>,
) => {
  const gateway = new FakeGateway()
  options.setup?.(gateway)
  const layer = St3Live({
    baseUrl: 'http://gateway.test',
    maxFollows: options.maxFollows,
    ...(options.conversationSlots === undefined ? {} : { conversationSlots: options.conversationSlots }),
    socket: gateway.factory,
    fetch: gateway.fetch,
    ...(options.onDiagnostics === undefined ? {} : { onDiagnostics: options.onDiagnostics }),
  })
  return Effect.gen(function* () {
    yield* settle
    yield* test(gateway)
  }).pipe(Effect.provide(layer), Effect.scoped)
}

const run = (
  options: Parameters<typeof runEffect>[0],
  test: Parameters<typeof runEffect>[1],
) => Effect.runPromise(runEffect(options, test))

describe('follow freshness', () => {
  beforeEach(() => vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] }))
  afterEach(() => vi.useRealTimers())

  it('resubscribes a cursor-gap conversation resync without failing or reconnecting the socket', () =>
    run({ maxFollows: 4 }, (gateway) =>
      Effect.gen(function* () {
        const ref = 'agent/cursor-gap'
        const conversation = yield* mountConversation(ref)
        const id = gateway.conversationId(ref)
        const socket = gateway.socket
        const frame: CollectionFrame = {
          kind: 'conversation', id, collection: 'conversation',
          session_id: 'session/cursor-gap', items: [], replace: true, has_more: false,
        }
        gateway.send(frame)
        yield* settle
        expect(conversation.lastFreshness()).toMatchObject({ _tag: 'Live' })
        gateway.send({
          kind: 'resync', id, collection: 'conversation', retryable: true,
          code: 'cursor-gap', message: 'The conversation cursor is no longer retained',
        })
        yield* settle
        expect(conversation.lastFreshness()).toMatchObject({
          _tag: 'Stale',
          reason: { _tag: 'Resync', code: 'cursor-gap', message: 'The conversation cursor is no longer retained' },
        })
        expect(conversation.events.at(-1)).toEqual({
          _tag: 'Stale', code: 'cursor-gap', message: 'The conversation cursor is no longer retained',
        })
        yield* Effect.promise(() => vi.advanceTimersByTimeAsync(1000))
        yield* settle
        expect(gateway.commands.filter(command => command.kind === 'subscribe' && command.conversation === ref)).toEqual([
          { kind: 'subscribe', id, collection: 'conversation', conversation: ref },
          { kind: 'subscribe', id: `${id}.2`, collection: 'conversation', conversation: ref },
        ])
        expect(gateway.unsubscribed()).toEqual([id])
        expect(gateway.socket).toBe(socket)
        expect(conversation.ended()).toBe(false)
        gateway.send({ ...frame, id: `${id}.2` })
        yield* settle
        expect(conversation.lastFreshness()).toMatchObject({ _tag: 'Live' })
        expect(conversation.events.map(event => event._tag)).toEqual(['Observed', 'Stale', 'Observed'])
      }),
    ))

  it('reports Requested only after the subscribe send and Live only after a real frame', () =>
    run({ maxFollows: 4 }, (gateway) =>
      Effect.gen(function* () {
        const agents = yield* mountWindow('agents')
        expect(agents.tags()).toEqual(['Requested'])
        gateway.window('agents')
        yield* settle
        expect(agents.tags()).toEqual(['Requested', 'Live'])
        expect(agents.lastFreshness()).toMatchObject({ _tag: 'Live' })
        yield* agents.interrupt
      }),
    ))

  it('shows Reconnecting and never Live while the socket is down, then Live after resubscribe', () =>
    run({ maxFollows: 4 }, (gateway) =>
      Effect.gen(function* () {
        const agents = yield* mountWindow('agents')
        gateway.window('agents')
        yield* settle
        gateway.end()
        yield* settle
        const tags = agents.tags()
        const downAt = tags.lastIndexOf('Reconnecting')
        expect(downAt).toBeGreaterThan(0)
        expect(tags.indexOf('Live', downAt)).toBe(-1)
        expect(agents.lastFreshness()).toMatchObject({ _tag: 'Reconnecting', attempt: 1 })
        // The channel backs off 500 ms, probes, reopens and resubscribes.
        yield* Effect.promise(async () => {
          for (
            let round = 0;
            round < 50 && agents.tags().indexOf('Requested', downAt) === -1;
            round += 1
          ) {
            await vi.advanceTimersByTimeAsync(50)
            await new Promise<void>((resolve) => setImmediate(resolve))
          }
        })
        yield* settle
        expect(agents.tags().indexOf('Requested', downAt)).toBeGreaterThan(downAt)
        gateway.window('agents')
        yield* settle
        expect(gateway.commands.filter((command) => command.kind === 'subscribe')).toHaveLength(2)
        expect(agents.lastFreshness()).toMatchObject({ _tag: 'Live' })
      }),
    ))

  it('ends with Stale(Evicted) when the conversation pool reclaims the slot', () =>
    run({ maxFollows: 8, conversationSlots: 2 }, (gateway) =>
      Effect.gen(function* () {
        const st3 = yield* St3
        const first = yield* mountConversation('agent/first')
        yield* st3.setVisible({ _tag: 'Conversation', ref: 'agent/first' }, false)
        yield* mountConversation('agent/second')
        const third = yield* mountConversation('agent/third')

        expect(first.ended()).toBe(true)
        expect(first.events.at(-1)?._tag).toBe('Stale')
        expect(first.lastFreshness()).toEqual({ _tag: 'Stale', reason: { _tag: 'Evicted' } })
        expect(gateway.unsubscribed()).toEqual([gateway.conversationId('agent/first')])
        expect(third.ended()).toBe(false)
      }),
    ))

  it('does not let window follows take conversation slots nor conversations take window slots', () =>
    run({ maxFollows: 5, conversationSlots: 2 }, (gateway) =>
      Effect.gen(function* () {
        const st3 = yield* St3
        const missions = yield* mountWindow('missions')
        yield* st3.setVisible(windowSpec('missions'), false)
        yield* st3.setVisible(windowSpec('missions'), true)
        const first = yield* mountConversation('agent/first')
        const second = yield* mountConversation('agent/second')
        yield* st3.setVisible({ _tag: 'Conversation', ref: 'agent/second' }, false)
        yield* mountWindow('agents')
        yield* mountWindow('attention')
        // The conversation pool is full (one visible, one invisible); the next conversation
        // evicts the invisible conversation, never a shared window slot.
        yield* mountConversation('agent/third')
        expect(second.ended()).toBe(true)
        expect(first.ended()).toBe(false)
        expect(gateway.unsubscribed()).toEqual([gateway.conversationId('agent/second')])
        expect(missions.ended()).toBe(false)

        // The shared pool is full with visible windows; a new window is refused rather
        // than taking a reserved conversation slot.
        const work = yield* mountWindow('work')
        expect(work.events.at(-1)).toMatchObject({
          _tag: 'Failed',
          error: { _tag: 'SubscriptionLimit', cap: 5 },
        })
        expect(gateway.unsubscribed()).toEqual([gateway.conversationId('agent/second')])
      }),
    ))

  it('clears the freshness table when every follow ends', () =>
    run({ maxFollows: 4 }, (gateway) =>
      Effect.gen(function* () {
        const st3 = yield* St3
        const agents = yield* mountWindow('agents')
        expect((yield* SubscriptionRef.get(st3.freshness)).size).toBe(1)
        yield* agents.interrupt
        yield* settle
        expect((yield* SubscriptionRef.get(st3.freshness)).size).toBe(0)
        expect(gateway.unsubscribed()).toEqual([gateway.idOf('agents')])
      }),
    ))

  it.each([401, 403])('settles the fallback budget and ends follows after a %s probe refusal', (status) => {
    const diagnostics: St3Diagnostic[] = []
    let refuseProbe: (() => void) | undefined
    return run({
      maxFollows: 8,
      conversationSlots: 'advertised',
      setup: (gateway) => {
        gateway.probe = () => new Promise<Response>((resolve) => {
          refuseProbe = () => resolve(new Response(JSON.stringify({
            api_version: 'st3.client.v0', snapshot,
            error: { code: 'forbidden', message: 'probe refused' },
          }), { status, headers: { 'content-type': 'application/json' } }))
        })
      },
      onDiagnostics: (event) => diagnostics.push(event),
    }, () => Effect.gen(function* () {
      const st3 = yield* St3
      const follow = yield* mountConversation('agent/refused')
      expect(follow.ended()).toBe(false)
      expect(refuseProbe).toBeDefined()
      refuseProbe!()
      yield* settle
      expect(diagnostics).toContainEqual({
        _tag: 'Follows', active: 0, cap: 8, conversationSlots: 4,
      })
      expect(follow.events).toMatchObject([{ _tag: 'Failed', error: { _tag: 'Rejected' } }])
      expect(follow.ended()).toBe(true)
      // Registration after the permanent rejection must fail too, not await a new socket.
      const later = yield* mountConversation('agent/later-refused')
      expect(later.events).toMatchObject([{ _tag: 'Failed', error: { _tag: 'Rejected' } }])
      expect(later.ended()).toBe(true)
      expect((yield* SubscriptionRef.get(st3.freshness)).size).toBe(0)
    }))
  })

  it('settles an 8/4 fallback after a transient failed probe and proceeds on retry', () => {
    const diagnostics: St3Diagnostic[] = []
    return run({
      maxFollows: 8, conversationSlots: 'advertised',
      setup: (gateway) => {
        gateway.probe = async () => {
          gateway.probe = undefined
          throw new Error('temporary probe outage')
        }
      },
      onDiagnostics: (event) => diagnostics.push(event),
    }, (gateway) => Effect.gen(function* () {
      const follow = yield* mountConversation('agent/retry')
      expect(diagnostics).toContainEqual({
        _tag: 'Follows', active: 1, cap: 8, conversationSlots: 4,
      })
      yield* Effect.promise(() => vi.advanceTimersByTimeAsync(500))
      yield* settle
      expect(gateway.conversationId('agent/retry')).toBeTruthy()
      expect(follow.tags()).toContain('Requested')
      expect(follow.ended()).toBe(false)
    }))
  })

  it('cleans a follow interrupted while the first probe holds admission', () => {
    let rejectProbe: ((error: Error) => void) | undefined
    return run({
      maxFollows: 8, conversationSlots: 'advertised',
      setup: (gateway) => {
        gateway.probe = () => new Promise<Response>((_, reject) => { rejectProbe = reject })
      },
    }, (gateway) => Effect.gen(function* () {
      const st3 = yield* St3
      const follow = yield* mountConversation('agent/cancelled')
      yield* follow.interrupt
      expect(follow.ended()).toBe(true)
      expect(rejectProbe).toBeDefined()
      gateway.probe = undefined
      rejectProbe!(new Error('first probe failed after cancellation'))
      yield* settle
      // A leaked freshener receives SocketDropped from Reconnecting and recreates this key.
      expect((yield* SubscriptionRef.get(st3.freshness)).size).toBe(0)
      yield* Effect.promise(() => vi.advanceTimersByTimeAsync(500))
      yield* settle
      expect(gateway.commands).toEqual([])
      gateway.end()
      yield* settle
      expect((yield* SubscriptionRef.get(st3.freshness)).size).toBe(0)
    }))
  })

  it('preserves admitted keys and LRU visibility across a successful reconnect probe', () =>
    run({ maxFollows: 8, conversationSlots: 'advertised' }, (gateway) =>
      Effect.gen(function* () {
        const st3 = yield* St3
        const first = yield* mountConversation('agent/oldest')
        yield* st3.setVisible({ _tag: 'Conversation', ref: 'agent/oldest' }, false)
        const second = yield* mountConversation('agent/second')
        yield* st3.setVisible({ _tag: 'Conversation', ref: 'agent/second' }, false)
        const third = yield* mountConversation('agent/third')
        yield* st3.setVisible({ _tag: 'Conversation', ref: 'agent/third' }, false)
        const fourth = yield* mountConversation('agent/fourth')
        gateway.end()
        yield* settle
        yield* Effect.promise(() => vi.advanceTimersByTimeAsync(500))
        yield* settle
        expect(gateway.commands.filter((c) => c.kind === 'subscribe')).toHaveLength(8)
        yield* mountConversation('agent/next')
        expect(first.ended()).toBe(true)
        expect(second.ended()).toBe(false)
        expect(third.ended()).toBe(false)
        expect(fourth.ended()).toBe(false)
        expect(gateway.unsubscribed()).toEqual([gateway.conversationId('agent/oldest')])
      }),
    ))

  it('shrinks a reconnect budget in place by evicting the oldest invisible follows', () =>
    run({
      maxFollows: 8, conversationSlots: 'advertised',
      setup: (gateway) => { gateway.collectionsV1 = true },
    }, (gateway) => Effect.gen(function* () {
      const st3 = yield* St3
      const follows: Mounted<ConversationChunk>[] = []
      for (let index = 0; index < 6; index += 1) {
        follows.push(yield* mountConversation(`agent/shrink-${index}`))
        if (index < 5)
          yield* st3.setVisible({ _tag: 'Conversation', ref: `agent/shrink-${index}` }, false)
      }
      gateway.collectionsV1 = false
      gateway.end()
      yield* settle
      yield* Effect.promise(() => vi.advanceTimersByTimeAsync(500))
      yield* settle
      expect(follows.map((follow) => follow.ended())).toEqual([true, true, false, false, false, false])
      yield* mountConversation('agent/after-shrink')
      expect(follows[2]!.ended()).toBe(true)
      expect(follows[5]!.ended()).toBe(false)
    })))

  it('keeps visible follows on budget shrink and trims them when they become invisible', () =>
    run({
      maxFollows: 8, conversationSlots: 'advertised',
      setup: (gateway) => { gateway.collectionsV1 = true },
    }, (gateway) => Effect.gen(function* () {
      const st3 = yield* St3
      const follows: Mounted<ConversationChunk>[] = []
      for (let index = 0; index < 6; index += 1)
        follows.push(yield* mountConversation(`agent/visible-${index}`))
      gateway.collectionsV1 = false
      gateway.end()
      yield* settle
      yield* Effect.promise(() => vi.advanceTimersByTimeAsync(500))
      yield* settle
      expect(follows.every((follow) => !follow.ended())).toBe(true)
      const refused = yield* mountConversation('agent/over-cap')
      expect(refused.events).toMatchObject([
        { _tag: 'Failed', error: { _tag: 'SubscriptionLimit', cap: 8 } },
      ])
      for (let index = 0; index < 2; index += 1)
        yield* st3.setVisible({ _tag: 'Conversation', ref: `agent/visible-${index}` }, false)
      yield* settle
      expect(follows.map((follow) => follow.ended())).toEqual([true, true, false, false, false, false])
      yield* st3.setVisible({ _tag: 'Conversation', ref: 'agent/visible-2' }, false)
      yield* mountConversation('agent/now-admitted')
      expect(follows[2]!.ended()).toBe(true)
      expect(follows[5]!.ended()).toBe(false)
    })))

  effectIt.live('keeps roster, pooled conversation and gateway non-live until decoded data after open and reconnect', () =>
    runEffect({ maxFollows: 8, conversationSlots: 2, setup: (gateway) => { gateway.autoOpen = false } }, (gateway) =>
      Effect.gen(function* () {
        const st3 = yield* St3
        const gatewayStatuses: SyncStatus[] = []
        yield* Effect.forkChild(st3.gatewaySyncStatus.pipe(
          Stream.runForEach((status) => Effect.sync(() => gatewayStatuses.push(status))),
        ))
        const roster = yield* mountWindow('agents')
        const conversation = yield* mountConversation('agent/sync-status')
        expect(roster.statuses.some((status) => status._tag === 'Live')).toBe(false)
        expect(conversation.statuses.some((status) => status._tag === 'Live')).toBe(false)
        gateway.open()
        yield* settle
        expect(roster.lastStatus()).toMatchObject({ _tag: 'Requested' })
        expect(conversation.lastStatus()).toMatchObject({ _tag: 'Requested' })
        expect(gatewayStatuses.at(-1)).toMatchObject({ _tag: 'Requested' })

        // A valid wire frame routed to the wrong protocol must not grant that follow Live.
        gateway.send({
          kind: 'conversation', id: gateway.idOf('agents'), collection: 'conversation',
          session_id: 'session/wrong-kind', items: [], replace: true, has_more: false,
        })
        yield* settle
        expect(roster.lastStatus()).toMatchObject({ _tag: 'Requested' })
        expect(gatewayStatuses.at(-1)).toMatchObject({ _tag: 'Requested' })
        gateway.window('agents')
        gateway.send({
          kind: 'conversation', id: gateway.conversationId('agent/sync-status'), collection: 'conversation',
          session_id: 'session/sync-status', items: [], replace: true, has_more: false,
        })
        yield* settle
        expect(roster.lastStatus()).toMatchObject({ _tag: 'Live' })
        expect(conversation.lastStatus()).toMatchObject({ _tag: 'Live' })
        expect(gatewayStatuses.at(-1)).toMatchObject({ _tag: 'Live' })
        gateway.end()
        yield* settle
        for (const status of [roster.lastStatus(), conversation.lastStatus(), gatewayStatuses.at(-1)]) {
          expect(status).toMatchObject({
            _tag: 'Stale', reason: { _tag: 'Reconnecting', attempt: 1, nextAt: expect.any(Number) },
          })
        }
        const rosterDown = roster.statuses.length
        const conversationDown = conversation.statuses.length
        const gatewayDown = gatewayStatuses.length
        yield* Effect.promise(() => vi.advanceTimersByTimeAsync(500))
        yield* settle
        gateway.open()
        yield* settle
        for (const statuses of [
          roster.statuses.slice(rosterDown),
          conversation.statuses.slice(conversationDown),
          gatewayStatuses.slice(gatewayDown),
        ]) expect(statuses.some((status) => status._tag === 'Live')).toBe(false)
        expect(roster.lastStatus()).toMatchObject({ _tag: 'Requested' })
        expect(conversation.lastStatus()).toMatchObject({ _tag: 'Requested' })
        expect(gatewayStatuses.at(-1)).toMatchObject({ _tag: 'Requested' })
        gateway.window('agents')
        yield* settle
        expect(roster.lastStatus()).toMatchObject({ _tag: 'Live' })
        expect(gatewayStatuses.at(-1)).toMatchObject({ _tag: 'Live' })
        expect(conversation.lastStatus()).toMatchObject({ _tag: 'Requested' })
        gateway.send({
          kind: 'conversation', id: gateway.conversationId('agent/sync-status'), collection: 'conversation',
          session_id: 'session/sync-status', items: [], replace: true, has_more: false,
        })
        yield* settle
        expect(conversation.lastStatus()).toMatchObject({ _tag: 'Live' })
      }),
    ))

  effectIt.live('retains exact local cap failure for the running status consumer', () =>
    runEffect({ maxFollows: 2 }, () =>
      Effect.gen(function* () {
        const st3 = yield* St3
        yield* mountWindow('agents')
        yield* mountWindow('missions')
        const refused = yield* mountWindow('attention')
        const expected: SyncStatus = {
          _tag: 'Failed', cause: { _tag: 'Local', kind: 'subscription-limit', detail: { cap: 2 } },
        }
        expect(refused.lastStatus()).toEqual(expected)
        expect((yield* SubscriptionRef.get(st3.syncStatuses)).get(followKey(windowSpec('attention')))).toEqual(expected)
        yield* refused.interrupt
        yield* settle
        expect((yield* SubscriptionRef.get(st3.syncStatuses)).has(followKey(windowSpec('attention')))).toBe(false)
      }),
    ))

  effectIt.live('does not invent a local cap cause for an uncoded legacy server limit', () =>
    runEffect({ maxFollows: 4 }, (gateway) =>
      Effect.gen(function* () {
        const follow = yield* mountWindow('agents')
        gateway.send({ kind: 'error', id: gateway.idOf('agents'), message: 'server has no slots' })
        yield* settle
        // The retry resend is the latest verdict; the uncoded limit itself must surface as a
        // metadata-less stale, never a fabricated Local/Server failure cause.
        expect(follow.statuses).toContainEqual({ _tag: 'Stale', reason: { _tag: 'Unknown' } })
        expect(follow.lastStatus()).toMatchObject({ _tag: 'Requested' })
        expect(follow.statuses.some((status) => status._tag === 'Failed')).toBe(false)
      }),
    ))

  effectIt.live.each([
    {
      code: 'forbidden' as const,
      expected: { _tag: 'Failed', cause: { _tag: 'Server', code: 'forbidden', message: 'exact refusal' } },
    },
    {
      code: 'subscription-limit' as const,
      expected: { _tag: 'Failed', cause: { _tag: 'Server', code: 'subscription-limit', message: 'exact refusal' } },
    },
    // Uncoded errors are retried, so their honest status is a metadata-less stale, not a fabricated cause.
    { code: undefined, expected: { _tag: 'Stale', reason: { _tag: 'Unknown' } } },
  ] as const)('maps scripted server refusal metadata honestly: $code', ({ code, expected }) =>
    runEffect({ maxFollows: 4 }, (gateway) =>
      Effect.gen(function* () {
        const follow = yield* mountWindow('agents')
        gateway.send({
          kind: 'error', id: gateway.idOf('agents'), collection: 'agents',
          message: 'exact refusal', ...(code === undefined ? {} : { code }),
        })
        yield* settle
        expect(follow.lastStatus()).toEqual(expected)
      }),
    ))

  effectIt.live('maps a scripted resync without metadata to Unknown rather than fabricated diagnostics', () =>
    runEffect({ maxFollows: 4 }, (gateway) =>
      Effect.gen(function* () {
        const follow = yield* mountWindow('agents')
        gateway.window('agents')
        yield* settle
        gateway.send({ kind: 'resync', id: gateway.idOf('agents'), collection: 'agents' })
        yield* settle
        expect(follow.lastStatus()).toEqual({
          _tag: 'Stale', reason: { _tag: 'Unknown' }, lastLiveAt: expect.any(Number),
        })
      }),
    ))
  effectIt.live('refreshes Live window snapshot evidence without restarting its since clock', () =>
    runEffect({ maxFollows: 4 }, (gateway) =>
      Effect.gen(function* () {
        const st3 = yield* St3
        const roster = yield* mountWindow('agents')
        gateway.window('agents')
        yield* settle
        const first = roster.lastStatus()
        expect(first).toMatchObject({ _tag: 'Live', snapshot })
        const updated = { ...snapshot, id: 'snapshot/2', store_index: 2 }
        gateway.send({
          kind: 'changes', id: gateway.idOf('agents'), collection: 'agents',
          upserts: [], removes: [], order: [], has_more: false, snapshot: updated,
        })
        yield* settle
        expect(roster.events.at(-1)).toMatchObject({ _tag: 'Observed', value: { snapshot: updated } })
        expect(roster.lastStatus()).toEqual({ ...first, snapshot: updated })
        expect((yield* SubscriptionRef.get(st3.syncStatuses)).get(followKey(windowSpec('agents'))))
          .toEqual({ ...first, snapshot: updated })
      }),
    ))

  it('binds a conversation chunk to the subscribe generation that delivered it', () =>
    run({ maxFollows: 4 }, (gateway) =>
      Effect.gen(function* () {
        const ref = 'agent/generation'
        const follow = yield* mountConversation(ref)
        const page = (id: string) =>
          gateway.send({
            kind: 'conversation', id, collection: 'conversation',
            session_id: 'session/1', replace: true, has_more: false, items: [],
          })
        // A transient resync schedules a resubscribe one second out.
        gateway.send({
          kind: 'resync', id: gateway.conversationId(ref), collection: 'conversation', retryable: true,
          code: 'cursor-gap', message: 'The conversation cursor is no longer retained',
        })
        yield* settle
        // The socket drops and reopens before that retry: the reopen subscribes at once.
        gateway.end()
        yield* Effect.promise(() => vi.advanceTimersByTimeAsync(500))
        yield* settle
        const reopened = gateway.conversationId(ref)
        // The still-pending retry then subscribes the conversation once more.
        yield* Effect.promise(() => vi.advanceTimersByTimeAsync(500))
        yield* settle
        const subscribes = gateway.commands.filter(
          (command) => command.kind === 'subscribe' && command.conversation === ref,
        )
        expect(subscribes).toHaveLength(3)
        const current = gateway.conversationId(ref)
        // The superseded subscription is released, so the gateway stops its follower.
        expect(gateway.unsubscribed()).toContain(reopened)
        // A page the reopened subscription read before the retry replaced it arrives late:
        // it must never be routed as a chunk of the newer subscription.
        page(reopened)
        yield* settle
        expect(follow.events.filter((event) => event._tag === 'Observed')).toEqual([])
        page(current)
        yield* settle
        expect(follow.events.at(-1)).toMatchObject({
          _tag: 'Observed',
          value: { id: current },
        })
        yield* follow.interrupt
      }),
    ))
})
