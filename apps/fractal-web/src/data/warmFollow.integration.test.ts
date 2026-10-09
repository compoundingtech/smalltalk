/**
 * Warm conversation switching at the data layer, against a scripted gateway through the real
 * SDK and liveSource. One test records deterministic data-layer latency numbers (warm
 * switch-back vs fresh switch, frame→fold→selector); the others pin the bounded live-follow
 * set: LRU eviction with a real unsubscribe, re-follow after eviction, reconnect honesty
 * (Reconnecting, never Live), and no leaked subscriptions.
 */
import type {
  Agent,
  CollectionCommand,
  CollectionFrame,
  CollectionSocket,
  Snapshot,
  TimelineEntry,
} from '@smalltalk/st3-client'
import * as Option from 'effect/Option'
import * as os from 'node:os'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { getDebug } from '../telemetry/measurement/index.ts'

import { liveSource, type LiveSource } from './liveSource.ts'
import type { ConversationPage, Feed } from './source.ts'

const snapshot: Snapshot = {
  id: 'snapshot/1',
  created_at: '2026-10-03T00:00:00Z',
  host_id: 'host/example',
  projection_version: 'client-projection.v0',
  store_index: 1,
}

const agent: Agent = {
  id: 'agent/example',
  kind: 'agent',
  revision: '1',
  updated_at: snapshot.created_at,
  name: 'Example',
  state: 'running',
  reachability: 'local',
  runtime_ids: ['runtime/old'],
}

const entry = (sequence: number, text: string): TimelineEntry => ({
  id: `timeline-entry/${sequence}`,
  sequence,
  revision: 1,
  type: 'content',
  role: 'assistant',
  final: true,
  timestamp: snapshot.created_at,
  body: { text, media_type: 'text/markdown' },
})

/** Record deterministic warm-switch timings without creating local artifacts. */
const recordMeasurement = (report: Record<string, unknown>) => {
  console.log(`warm-switch-measurement ${JSON.stringify(report)}`)
}

class Gateway {
  socket: CollectionSocket | undefined
  readonly commands: CollectionCommand[] = []
  readonly decodeMs: number[] = []
  /** Advertise the st main's `collections` v1 capability (subscription cap 16) when set. */
  collectionsV1 = false
  onUnsubscribe: (() => void) | undefined
  denyReads = false
  publishedRoster: Agent[] | undefined
  publishedHasMore = false
  rosterFailure: 403 | 503 | undefined
  rosterGate: Promise<void> | undefined
  readonly requests: URL[] = []

  readonly fetch: typeof fetch = async (input) => {
    const url = new URL(String(input))
    const path = url.pathname
    this.requests.push(url)
    if (path === '/v1/client/agents' && this.rosterFailure !== undefined) {
      await this.rosterGate
      return new Response(JSON.stringify({
        api_version: 'st3.client.v0', error_version: 'st3.client.error.v0',
        code: this.rosterFailure === 403 ? 'forbidden' : 'unavailable',
        message: 'Publication unavailable', retryable: this.rosterFailure === 503,
        request_id: 'request/roster', details: {},
      }), { status: this.rosterFailure, headers: { 'content-type': 'application/json' } })
    }
    if (path === '/v1/client/agents' && this.publishedRoster !== undefined) {
      await this.rosterGate
      return new Response(JSON.stringify({ api_version: 'st3.client.v0', snapshot, value: {
        kind: 'page', collection: 'agents', filters: {}, items: this.publishedRoster,
        page: { limit: 100, has_more: this.publishedHasMore, next_cursor: this.publishedHasMore ? 'cursor/roster' : null },
      } }), { headers: { 'content-type': 'application/json' } })
    }
    if (path !== '/v1/client/capabilities') throw new Error(`Unexpected request ${path}`)
    if (this.denyReads) return new Response(JSON.stringify({
      api_version: 'st3.client.v0', error_version: 'st3.client.error.v0',
      code: 'forbidden', message: 'Access denied', retryable: false,
      request_id: 'request/refused', details: {},
    }), { status: 403, headers: { 'content-type': 'application/json' } })
    return new Response(
      JSON.stringify({
        api_version: 'st3.client.v0',
        snapshot,
        value: {
          kind: 'capabilities',
          capabilities: [
            { id: 'work.done', state: 'granted', version: 0 },
            ...(this.collectionsV1
              ? [{ id: 'collections', state: 'granted' as const, version: 1 }]
              : []),
          ],
          event_cursor: 'cursor/current',
          limits: {
            max_page_items: 100,
            max_event_items: 100,
            max_wait_ms: 1000,
            max_response_bytes: 65536,
          },
          schemas: ['client-v0'],
          session_actor: 'person/operator',
          transport: 'fabric-loopback',
        },
      }),
      { status: 200, headers: { 'content-type': 'application/json' } },
    )
  }

  readonly factory = () => {
    const socket: CollectionSocket = {
      onopen: null,
      onmessage: null,
      onclose: null,
      onerror: null,
      send: (text: string) => {
        const command: CollectionCommand = JSON.parse(text)
        this.commands.push(command)
        if (command.kind === 'unsubscribe') this.onUnsubscribe?.()
      },
      close: () => {},
    }
    this.socket = socket
    queueMicrotask(() => socket.onopen?.())
    return socket
  }

  conversationSubscription(ref: string) {
    const command = this.commands.findLast(
      (candidate): candidate is Extract<CollectionCommand, { kind: 'subscribe'; collection: 'conversation' }> =>
        candidate.kind === 'subscribe' && candidate.collection === 'conversation' && candidate.conversation === ref,
    )
    if (command === undefined) throw new Error(`No conversation subscription for ${ref}`)
    return command
  }

  subscribesFor(ref: string) {
    return this.commands.filter(
      (candidate) =>
        candidate.kind === 'subscribe' &&
        candidate.collection === 'conversation' &&
        candidate.conversation === ref,
    ).length
  }

  unsubscribedIds() {
    return this.commands.filter((command) => command.kind === 'unsubscribe').map((c) => c.id)
  }

  send(frame: CollectionFrame) {
    this.socket?.onmessage?.({ data: JSON.stringify(frame) })
  }

  conversationFrame(ref: string, rows: TimelineEntry[], replace: boolean) {
    this.send({
      kind: 'conversation',
      id: this.conversationSubscription(ref).id,
      collection: 'conversation',
      session_id: `session/${ref}`,
      items: rows,
      replace,
      has_more: false,
    })
  }

  fleet(rows: Agent[]) {
    const command = this.commands.findLast(
      (candidate) => candidate.kind === 'subscribe' && candidate.collection === 'agents',
    )
    if (command === undefined) throw new Error('No agents subscription')
    this.send({
      kind: 'snapshot',
      id: command.id,
      collection: 'agents',
      has_more: false,
      items: rows,
      order: rows.map((row) => row.id),
      snapshot,
    })
  }
}

let frames = new Map<number, FrameRequestCallback>()
let flushMs = 0
beforeEach(() => {
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
  frames = new Map()
  flushMs = 0
  let nextFrame = 0
  vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
    const id = (nextFrame += 1)
    frames.set(id, callback)
    return id
  })
  vi.stubGlobal('cancelAnimationFrame', (id: number) => frames.delete(id))
})
afterEach(() => {
  vi.unstubAllGlobals()
  vi.useRealTimers()
})

/** Drain microtasks, timers and one animation-frame commit; accumulates raf execution time. */
const drain = async () => {
  await new Promise<void>((resolve) => setImmediate(resolve))
  await vi.advanceTimersByTimeAsync(1)
  const pending = [...frames.values()]
  frames.clear()
  const started = performance.now()
  for (const callback of pending) callback(0)
  flushMs += performance.now() - started
}

const until = async (check: () => boolean, maxRounds = 400) => {
  for (let round = 0; round < maxRounds && !check(); round += 1) await drain()
  if (!check()) throw new Error('condition not reached before the drain budget ran out')
}

const openLive = ({ maxFollows, conversationSlots, denyReads = false }: { maxFollows: number; conversationSlots?: number | 'advertised'; denyReads?: boolean }) => {
  const gateway = new Gateway()
  gateway.denyReads = denyReads
  const options = {
    baseUrl: 'http://gateway.test',
    maxFollows,
    socket: gateway.factory,
    fetch: gateway.fetch,
    onDiagnostics: (event: { _tag: string; elapsedMs?: number }) => {
      if (event._tag === 'Decode' && event.elapsedMs !== undefined) gateway.decodeMs.push(event.elapsedMs)
    },
    ...(conversationSlots === undefined ? {} : { conversationSlots }),
  }
  const live = liveSource({ options })
  return { live, gateway }
}

/** Mount the three standing window follows so conversations share the socket with real surfaces. */
const mountWindows = (live: LiveSource) => {
  const unmounts = [
    live.registry.mount(live.source.agents),
    live.registry.mount(live.source.missions),
    live.registry.mount(live.source.attention),
  ]
  return () => {
    for (const unmount of unmounts) unmount()
  }
}

const items = (feed: Feed<ConversationPage>) =>
  feed._tag === 'Observed' ? feed.value.items : undefined

const snapshotRows = (count: number) =>
  Array.from({ length: count }, (_, index) =>
    entry(index + 1, `Row ${index + 1} — planned work, review notes and a longer body to keep decode cost realistic.`),
  )

/** View one conversation: mount its interest, deliver the snapshot, then step away. */
const viewConversation = async (live: LiveSource, gateway: Gateway, ref: string, rows: TimelineEntry[]) => {
  const unmount = live.registry.mount(live.source.conversationInterest!(ref))
  await until(() => gateway.subscribesFor(ref) === 1)
  gateway.conversationFrame(ref, rows, true)
  await until(() => items(live.registry.get(live.source.conversation(ref)))?.length === rows.length)
  unmount()
  await drain()
}

const percentile = (values: number[], fraction: number) => {
  const sorted = [...values].sort((a, b) => a - b)
  return sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * fraction))]!
}

const summarize = (values: number[]) => ({
  median: percentile(values, 0.5),
  p95: percentile(values, 0.95),
  max: Math.max(...values),
})

describe('warm conversation switching at the data layer', () => {
  it('paints the published roster before any collection snapshot without asking for fresh=true', async () => {
    const { live, gateway } = openLive({ maxFollows: 8 })
    gateway.publishedRoster = [agent]
    const release = live.registry.mount(live.source.agents)
    try {
      await drain()
      await drain()
      await drain()
      const feed = live.registry.get(live.source.agents)
      expect(gateway.requests.filter(url => url.pathname === '/v1/client/agents')).toHaveLength(1)
      expect(gateway.requests.filter(url => url.pathname === '/v1/client/capabilities')).toHaveLength(1)
      expect(gateway.requests.find(url => url.pathname === '/v1/client/agents')?.search).toBe('?limit=100')
      expect(feed).toMatchObject({ _tag: 'Observed', freshness: 'stale', value: [{ id: agent.id }] })
      gateway.fleet([{ ...agent, name: 'Live roster' }])
      await until(() => {
        const current = live.registry.get(live.source.agents)
        return current._tag === 'Observed' && current.freshness === 'live' && current.value[0]?.name === 'Live roster'
      })
    } finally { release(); await live.dispose() }
  })

  it('keeps waiting for live data when no roster publication exists yet', async () => {
    const { live, gateway } = openLive({ maxFollows: 8 })
    gateway.rosterFailure = 503
    const release = live.registry.mount(live.source.agents)
    try {
      await drain()
      await drain()
      await drain()
      expect(live.registry.get(live.source.agents)).toEqual({ _tag: 'Waiting' })
      gateway.fleet([agent])
      await until(() => live.registry.get(live.source.agents)._tag === 'Observed')
    } finally { release(); await live.dispose() }
  })

  it('does not resurrect roster rows after the HTTP publication denies read access', async () => {
    const { live, gateway } = openLive({ maxFollows: 8 })
    let releaseRead: () => void = () => {}
    gateway.rosterGate = new Promise<void>(resolve => { releaseRead = resolve })
    gateway.rosterFailure = 403
    const release = live.registry.mount(live.source.agents)
    try {
      await until(() => gateway.commands.some(command => command.kind === 'subscribe' && command.collection === 'agents'))
      releaseRead()
      await until(() => live.registry.get(live.source.agents)._tag === 'Unavailable')
      gateway.fleet([agent])
      await drain()
      await drain()
      expect(live.registry.get(live.source.agents)).toMatchObject({ _tag: 'Unavailable', reason: 'ungranted' })
    } finally { releaseRead(); release(); await live.dispose() }
  })

  it('discloses a partial published page until the live roster arrives', async () => {
    const { live, gateway } = openLive({ maxFollows: 8 })
    gateway.publishedRoster = [agent]
    gateway.publishedHasMore = true
    const release = live.registry.mount(live.source.agents)
    try {
      await until(() => live.registry.get(live.source.agents)._tag === 'Observed')
      expect(live.registry.get(live.source.agents)).toMatchObject({ _tag: 'Observed', freshness: 'stale', coverage: { _tag: 'Partial' } })
      expect(gateway.requests.filter(url => url.pathname === '/v1/client/agents')).toHaveLength(1)
      gateway.fleet([agent])
      await until(() => {
        const feed = live.registry.get(live.source.agents)
        return feed._tag === 'Observed' && feed.freshness === 'live'
      })
      expect(live.registry.get(live.source.agents)).not.toHaveProperty('coverage')
    } finally { release(); await live.dispose() }
  })

  it('clears a live-first roster when the pending HTTP read later refuses access', async () => {
    const { live, gateway } = openLive({ maxFollows: 8 })
    let releaseRead: () => void = () => {}
    gateway.rosterGate = new Promise<void>(resolve => { releaseRead = resolve })
    gateway.rosterFailure = 403
    const release = live.registry.mount(live.source.agents)
    try {
      await until(() => gateway.commands.some(command => command.kind === 'subscribe' && command.collection === 'agents'))
      gateway.fleet([agent])
      await until(() => live.registry.get(live.source.agents)._tag === 'Observed')
      releaseRead()
      await until(() => live.registry.get(live.source.agents)._tag === 'Unavailable')
      gateway.fleet([agent])
      await drain()
      await drain()
      expect(live.registry.get(live.source.agents)).toMatchObject({ _tag: 'Unavailable', reason: 'ungranted' })
    } finally { releaseRead(); release(); await live.dispose() }
  })

  it('does not replace a live roster with a late older HTTP publication', async () => {
    const { live, gateway } = openLive({ maxFollows: 8 })
    gateway.publishedRoster = [agent]
    let releaseRead: () => void = () => {}
    gateway.rosterGate = new Promise<void>(resolve => { releaseRead = resolve })
    const release = live.registry.mount(live.source.agents)
    try {
      await until(() => gateway.commands.some(command => command.kind === 'subscribe' && command.collection === 'agents'))
      gateway.fleet([{ ...agent, name: 'Newer roster' }])
      await until(() => {
        const feed = live.registry.get(live.source.agents)
        return feed._tag === 'Observed' && feed.value[0]?.name === 'Newer roster'
      })
      releaseRead()
      await drain()
      await drain()
      expect(live.registry.get(live.source.agents)).toMatchObject({ _tag: 'Observed', freshness: 'live', value: [{ name: 'Newer roster' }] })
    } finally { releaseRead(); release(); await live.dispose() }
  })

  it('reveals an evicted cached transcript synchronously while its new read is still pending', async () => {
    const { live, gateway } = openLive({ maxFollows: 8, conversationSlots: 1 })
    try {
      await viewConversation(live, gateway, 'agent/cached', snapshotRows(3))
      await viewConversation(live, gateway, 'agent/other', snapshotRows(2))
      const before = gateway.subscribesFor('agent/cached')
      live.selectConversation('agent/cached')
      const feed = live.registry.get(live.source.conversation('agent/cached'))
      expect(feed).toMatchObject({ _tag: 'Observed', freshness: 'stale' })
      expect(items(feed)).toHaveLength(3)
      // No await or gateway response occurs between selection and this cached read.
      expect(gateway.subscribesFor('agent/cached')).toBe(before)
      await until(() => gateway.subscribesFor('agent/cached') === before + 1)
    } finally { await live.dispose() }
  })

  it('records warm switch-back and fresh switch data-layer latency against a scripted gateway', async () => {
    const iterations = 25
    const conversationCount = 200
    const policies = [
      { policy: 'shared-pool', conversationSlots: undefined },
      { policy: 'conversation-pool', conversationSlots: 4 },
    ]
    const report: Record<string, unknown> = {
      iterations,
      conversationEntries: conversationCount,
      loadAverage: os.loadavg(),
      uptimeSeconds: Math.round(os.uptime()),
      timestamp: new Date().toISOString(),
    }
    for (const { policy, conversationSlots } of policies) {
      const warmReads: number[] = []
      const warmDeltas: number[] = []
      const freshSwitches: number[] = []
      const freshSubscribes: number[] = []
      const decodes: number[] = []
      let commitMs = 0
      for (let index = 0; index < iterations; index += 1) {
        flushMs = 0
        const { live, gateway } = openLive({ maxFollows: 8, conversationSlots })
        const closeWindows = mountWindows(live)
        await until(() => gateway.commands.filter((c) => c.kind === 'subscribe' && c.collection).length >= 3)
        try {
          const warmRef = 'agent/warm'
          await viewConversation(live, gateway, warmRef, snapshotRows(conversationCount))
          const subscribesBefore = gateway.subscribesFor(warmRef)
          const unsubscribedBefore = gateway.unsubscribedIds().length

          // Switch back to the retained live follow: the selector must reflect the retained
          // snapshot synchronously, with no resubscribe on the wire.
          const readStarted = performance.now()
          const unmount = live.registry.mount(live.source.conversationInterest!(warmRef))
          const warm = live.registry.get(live.source.conversation(warmRef))
          const warmRead = performance.now() - readStarted
          expect(warm._tag).toBe('Observed')
          expect(items(warm)!.length).toBe(conversationCount)
          expect(gateway.subscribesFor(warmRef)).toBe(subscribesBefore)
          expect(gateway.unsubscribedIds().length).toBe(unsubscribedBefore)
          warmReads.push(warmRead)

          // A delta on the live follow: frame → fold → selector.
          const deltaStarted = performance.now()
          gateway.conversationFrame(warmRef, [entry(conversationCount + 1, 'Warm delta row')], false)
          await until(() => items(live.registry.get(live.source.conversation(warmRef)))?.length === conversationCount + 1)
          warmDeltas.push(performance.now() - deltaStarted)
          unmount()
          await drain()

          // A fresh switch: a conversation outside the live set re-follows from scratch.
          const freshRef = `agent/fresh-${index}`
          const subscribeStarted = performance.now()
          const freshUnmount = live.registry.mount(live.source.conversationInterest!(freshRef))
          await until(() => gateway.subscribesFor(freshRef) === 1)
          freshSubscribes.push(performance.now() - subscribeStarted)
          const frameStarted = performance.now()
          gateway.conversationFrame(freshRef, snapshotRows(conversationCount), true)
          await until(
            () => items(live.registry.get(live.source.conversation(freshRef)))?.length === conversationCount,
          )
          freshSwitches.push(performance.now() - frameStarted)
          freshUnmount()
          commitMs += flushMs
          decodes.push(...gateway.decodeMs)
        } finally {
          closeWindows()
          await live.dispose()
        }
      }
      report[policy] = {
        warmSwitchBackReadMs: summarize(warmReads),
        warmDeltaFrameToSelectorMs: summarize(warmDeltas),
        freshSwitchSubscribeSendMs: summarize(freshSubscribes),
        freshSwitchFrameToSelectorMs: summarize(freshSwitches),
        conversationDecodeMs: summarize(decodes),
        frameCommitMs: { total: commitMs },
      }
    }
    recordMeasurement(report)
  })

  it('keeps the most recently viewed conversations subscribed and evicts the least recent with a real unsubscribe', async () => {
    const { live, gateway } = openLive({ maxFollows: 8, conversationSlots: 2 })
    try {
      const closeWindows = mountWindows(live)
      await viewConversation(live, gateway, 'agent/c1', snapshotRows(3))
      await viewConversation(live, gateway, 'agent/c2', snapshotRows(3))
      const c1Id = gateway.conversationSubscription('agent/c1').id
      const c2Id = gateway.conversationSubscription('agent/c2').id

      // The conversation pool holds two; viewing a third evicts the least recently viewed one.
      await viewConversation(live, gateway, 'agent/c3', snapshotRows(3))

      expect(gateway.unsubscribedIds()).toEqual([c1Id])
      expect(gateway.unsubscribedIds()).not.toContain(c2Id)
      expect(getDebug('Wf.activeFollows')).toBe(5)
      const evicted = live.registry.get(live.source.conversation('agent/c1'))
      expect(evicted).toMatchObject({ _tag: 'Observed', freshness: 'stale' })
      closeWindows()
    } finally {
      await live.dispose()
    }
  })

  it('re-follows an evicted conversation as a fresh switch with a new subscription and Requested before Live', async () => {
    const { live, gateway } = openLive({ maxFollows: 8, conversationSlots: 2 })
    try {
      const closeWindows = mountWindows(live)
      await viewConversation(live, gateway, 'agent/c1', snapshotRows(3))
      await viewConversation(live, gateway, 'agent/c2', snapshotRows(3))
      await viewConversation(live, gateway, 'agent/c3', snapshotRows(3))
      expect(gateway.unsubscribedIds()).toEqual([gateway.conversationSubscription('agent/c1').id])

      const unmount = live.registry.mount(live.source.conversationInterest!('agent/c1'))
      await until(() => gateway.subscribesFor('agent/c1') === 2)
      const sync = live.registry.get(live.source.sync!.conversation('agent/c1'))
      expect(sync.sync.status).toEqual({ _tag: 'Requested', since: expect.any(Number) })
      gateway.conversationFrame('agent/c1', snapshotRows(4), true)
      await until(() => items(live.registry.get(live.source.conversation('agent/c1')))?.length === 4)
      expect(live.registry.get(live.source.sync!.conversation('agent/c1')).sync.status).toEqual({
        _tag: 'Live',
        since: expect.any(Number),
      })
      const resubscribed = gateway.conversationSubscription('agent/c1').id
      expect(gateway.unsubscribedIds()).not.toContain(resubscribed)
      unmount()
      closeWindows()
    } finally {
      await live.dispose()
    }
  })

  it('re-acquires a cold conversation while its evicted follow is still finalizing', async () => {
    const { live, gateway } = openLive({ maxFollows: 8, conversationSlots: 1 })
    try {
      const ref = 'agent/cold'
      const releaseCold = live.registry.mount(live.source.conversationInterest!(ref))
      await until(() => gateway.subscribesFor(ref) === 1)
      releaseCold()
      await drain()
      let releaseReplacement = () => {}
      let releaseReacquired = () => {}
      let reacquired = false
      // The unsubscribe is the deterministic barrier: the SDK has evicted the cold
      // follow, but its source stream/freshness finalizers have not finished yet.
      gateway.onUnsubscribe = () => {
        if (reacquired) return
        reacquired = true
        releaseReplacement()
        releaseReacquired = live.registry.mount(live.source.conversationInterest!(ref))
      }
      releaseReplacement = live.registry.mount(live.source.conversationInterest!('agent/replacement'))
      await until(() => reacquired)
      await until(() => gateway.subscribesFor(ref) === 2)
      gateway.conversationFrame(ref, snapshotRows(1), true)
      await until(() => items(live.registry.get(live.source.conversation(ref)))?.length === 1)
      releaseReacquired()
    } finally {
      await live.dispose()
    }
  })

  it('does not automatically refresh a visible conversation after connection rejection', async () => {
    const { live, gateway } = openLive({ maxFollows: 8, denyReads: true })
    const refresh = vi.spyOn(live.registry, 'refresh')
    const writes = vi.spyOn(live.registry, 'set')
    const queued: VoidFunction[] = []
    let restoreMicrotasks = () => {}
    try {
      await live.ready
      await until(() => getDebug('Wf.socketErrors') === 1)
      // Drain each handoff explicitly so a broken microtask loop is observable
      // as a growing counter instead of hanging the test runner.
      const microtasks = vi.spyOn(globalThis, 'queueMicrotask').mockImplementation(callback => queued.push(callback))
      restoreMicrotasks = () => microtasks.mockRestore()
      live.registry.mount(live.source.conversationInterest!('agent/refused'))
      await drain()
      // Every controller run creates one new refused Feed identity; its frame
      // publication reuses that identity and therefore does not inflate this count.
      const runs = () => new Set(writes.mock.calls.map(([, value]) => value).filter(value =>
        typeof value === 'object' && value !== null && '_tag' in value && value._tag === 'Unavailable',
      )).size
      const runCounts = [runs()]
      const counts = [refresh.mock.calls.length]
      for (let round = 0; round < 4; round += 1) {
        const pending = queued.splice(0)
        for (const callback of pending) callback()
        await drain()
        counts.push(refresh.mock.calls.length)
        runCounts.push(runs())
      }
      console.log(`connection-refused-refresh-counts ${JSON.stringify(counts)}`)
      console.log(`connection-refused-run-counts ${JSON.stringify(runCounts)}`)
      expect(counts).toEqual([0, 0, 0, 0, 0])
      expect(runCounts).toEqual([1, 1, 1, 1, 1])
      expect(gateway.commands).toHaveLength(0)
      expect(live.registry.get(live.source.conversation('agent/refused'))).toMatchObject({
        _tag: 'Unavailable', reason: 'ungranted',
      })
    } finally {
      restoreMicrotasks()
      refresh.mockRestore()
      writes.mockRestore()
      await live.dispose()
    }
  })

  it('re-follows a conversation whose follow ended in Failed when interest is re-acquired, keeping the last verified page', async () => {
    const { live, gateway } = openLive({ maxFollows: 8, conversationSlots: 4 })
    try {
      const closeWindows = mountWindows(live)
      const ref = 'agent/flaky'
      await viewConversation(live, gateway, ref, snapshotRows(2))
      // A non-authorization failure ends the follow; the retained page stays as trusted stale content.
      gateway.send({
        kind: 'error', id: gateway.conversationSubscription(ref).id,
        collection: 'conversation', code: 'unavailable',
        message: 'Owner host unavailable', retryable: false,
      })
      await until(() => live.registry.get(live.source.sync!.conversation(ref)).sync.status._tag === 'Failed')
      expect(live.registry.get(live.source.conversation(ref))).toMatchObject({
        _tag: 'Observed',
        freshness: 'stale',
        error: { reason: 'failed', detail: 'Owner host unavailable' },
      })

      // Switching back to this conversation is a fresh switch: re-acquired interest must
      // re-open the follow instead of showing the terminal failure until a reload.
      const unmount = live.registry.mount(live.source.conversationInterest!(ref))
      await until(() => gateway.subscribesFor(ref) === 2)
      expect(live.registry.get(live.source.sync!.conversation(ref)).sync.status._tag).not.toBe('Failed')
      gateway.conversationFrame(ref, snapshotRows(3), true)
      await until(() => items(live.registry.get(live.source.conversation(ref)))?.length === 3)
      expect(live.registry.get(live.source.conversation(ref))).toMatchObject({ _tag: 'Observed', freshness: 'live' })
      expect(live.registry.get(live.source.sync!.conversation(ref)).sync.status).toEqual({
        _tag: 'Live',
        since: expect.any(Number),
      })
      unmount()
      closeWindows()
    } finally {
      await live.dispose()
    }
  })

  describe('explicit conversation retry', () => {
    const subscriptionIds = (gateway: Gateway, ref: string) =>
      gateway.commands.flatMap((command) =>
        command.kind === 'subscribe' && command.collection === 'conversation' && command.conversation === ref ? [command.id] : [])
    const failFollow = async (live: LiveSource, gateway: Gateway, ref: string) => {
      gateway.send({
        kind: 'error', id: gateway.conversationSubscription(ref).id,
        collection: 'conversation', code: 'unavailable', message: 'Owner host unavailable', retryable: false,
      })
      await until(() => live.registry.get(live.source.sync!.conversation(ref)).sync.status._tag === 'Failed')
    }

    it('re-subscribes a failed follow exactly once for repeated synchronous retries', async () => {
      const { live, gateway } = openLive({ maxFollows: 8, conversationSlots: 4 })
      try {
        const closeWindows = mountWindows(live)
        const ref = 'agent/failing'
        const unmount = live.registry.mount(live.source.conversationInterest!(ref))
        await until(() => gateway.subscribesFor(ref) === 1)
        await failFollow(live, gateway, ref)
        expect(live.registry.get(live.source.conversation(ref))).toMatchObject({ _tag: 'Unavailable', reason: 'failed' })

        for (let index = 0; index < 5; index += 1) live.source.retryConversation!(ref)
        await until(() => gateway.subscribesFor(ref) === 2)
        // Barrier: the renewed follow is Live, so every forked re-subscribe has reached the socket.
        gateway.conversationFrame(ref, snapshotRows(2), true)
        await until(() => live.registry.get(live.source.sync!.conversation(ref)).sync.status._tag === 'Live')
        expect(gateway.subscribesFor(ref)).toBe(2)
        expect(gateway.unsubscribedIds()).not.toContain(gateway.conversationSubscription(ref).id)
        expect(items(live.registry.get(live.source.conversation(ref)))).toHaveLength(2)
        unmount()
        closeWindows()
      } finally {
        await live.dispose()
      }
    })

    it('leaves a healthy follow untouched: no unsubscribe, no new subscribe, no Waiting flash', async () => {
      const { live, gateway } = openLive({ maxFollows: 8, conversationSlots: 4 })
      try {
        const closeWindows = mountWindows(live)
        const ref = 'agent/healthy'
        const unmount = live.registry.mount(live.source.conversationInterest!(ref))
        await until(() => gateway.subscribesFor(ref) === 1)
        gateway.conversationFrame(ref, snapshotRows(2), true)
        await until(() => live.registry.get(live.source.sync!.conversation(ref)).sync.status._tag === 'Live')
        const seen: Array<Feed<ConversationPage>['_tag']> = []
        const stopWatching = live.registry.subscribe(live.source.conversation(ref), (feed) => seen.push(feed._tag), { immediate: true })

        for (let index = 0; index < 3; index += 1) live.source.retryConversation!(ref)
        // Barrier: a later frame on the same subscription lands, so any retry side effect has run.
        gateway.conversationFrame(ref, snapshotRows(3), true)
        await until(() => items(live.registry.get(live.source.conversation(ref)))?.length === 3)
        expect(gateway.subscribesFor(ref)).toBe(1)
        expect(gateway.unsubscribedIds()).toEqual([])
        expect(seen).not.toContain('Waiting')
        expect(seen.every((tag) => tag === 'Observed')).toBe(true)
        expect(live.registry.get(live.source.conversation(ref))).toMatchObject({ _tag: 'Observed', freshness: 'live' })
        stopWatching()
        unmount()
        closeWindows()
      } finally {
        await live.dispose()
      }
    })

    it('releases a retried follow on eviction and re-follows it on a later re-acquire and retry', async () => {
      const { live, gateway } = openLive({ maxFollows: 8, conversationSlots: 2 })
      try {
        const closeWindows = mountWindows(live)
        const ref = 'agent/retried'
        let unmount = live.registry.mount(live.source.conversationInterest!(ref))
        await until(() => gateway.subscribesFor(ref) === 1)
        await failFollow(live, gateway, ref)
        live.source.retryConversation!(ref)
        await until(() => gateway.subscribesFor(ref) === 2)
        gateway.conversationFrame(ref, snapshotRows(2), true)
        await until(() => live.registry.get(live.source.sync!.conversation(ref)).sync.status._tag === 'Live')
        const [failedId, retriedId] = subscriptionIds(gateway, ref)
        unmount()
        await drain()

        // Two newer conversations fill the two slots: the retried follow is evicted with a real unsubscribe.
        await viewConversation(live, gateway, 'agent/n1', snapshotRows(2))
        await viewConversation(live, gateway, 'agent/n2', snapshotRows(2))
        await until(() => gateway.unsubscribedIds().includes(retriedId!))
        await until(() => live.registry.get(live.source.sync!.conversation(ref)).sync.status._tag === 'Stale')
        // Balance: every subscription opened for this conversation, failed or retried, is released.
        expect(gateway.unsubscribedIds().filter((id) => subscriptionIds(gateway, ref).includes(id))).toEqual([failedId, retriedId])
        expect(getDebug('Wf.activeFollows')).toBe(5)

        // Re-acquired interest re-follows the evicted conversation.
        unmount = live.registry.mount(live.source.conversationInterest!(ref))
        await until(() => gateway.subscribesFor(ref) === 3)
        gateway.conversationFrame(ref, snapshotRows(3), true)
        await until(() => items(live.registry.get(live.source.conversation(ref)))?.length === 3)
        expect(live.registry.get(live.source.sync!.conversation(ref)).sync.status._tag).toBe('Live')

        // A renewed failure recovers through retry again.
        await failFollow(live, gateway, ref)
        live.source.retryConversation!(ref)
        await until(() => gateway.subscribesFor(ref) === 4)
        gateway.conversationFrame(ref, snapshotRows(4), true)
        await until(() => items(live.registry.get(live.source.conversation(ref)))?.length === 4)
        expect(live.registry.get(live.source.sync!.conversation(ref)).sync.status._tag).toBe('Live')
        unmount()
        closeWindows()
      } finally {
        await live.dispose()
      }
    })
  })

  it('shows Reconnecting and never Live while the socket is down, keeping the last value', async () => {
    const { live, gateway } = openLive({ maxFollows: 8, conversationSlots: 2 })
    try {
      const closeWindows = mountWindows(live)
      await viewConversation(live, gateway, 'agent/c1', snapshotRows(3))
      const syncAtom = live.source.sync!.conversation('agent/c1')
      expect(live.registry.get(syncAtom).sync.status).toEqual({ _tag: 'Live', since: expect.any(Number) })

      gateway.socket?.onclose?.({ code: 1006, reason: 'socket dropped' })
      await until(() => live.registry.get(syncAtom).sync.status._tag !== 'Live')
      const down = live.registry.get(syncAtom)
      expect(down.sync.status).toMatchObject({
        _tag: 'Stale',
        reason: { _tag: 'Reconnecting', attempt: 1, nextAt: expect.any(Number) },
      })
      expect(Option.isSome(down.last)).toBe(true)

      // The channel backs off 500 ms, probes, reopens and resubscribes every follow.
      await vi.advanceTimersByTimeAsync(600)
      await until(() => gateway.subscribesFor('agent/c1') === 2)
      // The actual resend re-grounds Requested; Live still waits for a real decoded frame.
      expect(live.registry.get(syncAtom).sync.status).toMatchObject({ _tag: 'Requested' })
      expect(live.registry.get(live.source.conversation('agent/c1'))).toMatchObject({ _tag: 'Observed', freshness: 'stale' })
      gateway.conversationFrame('agent/c1', snapshotRows(4), true)
      await until(() => items(live.registry.get(live.source.conversation('agent/c1')))?.length === 4)
      const recovered = live.registry.get(syncAtom)
      expect(recovered.sync.status).toEqual({ _tag: 'Live', since: expect.any(Number) })
      expect(Option.getOrUndefined(recovered.last)?.value.items.length).toBe(4)
      closeWindows()
    } finally {
      await live.dispose()
    }
  })

  it('marks cursor-gap resync stale and replaces retained history after the fresh collection read', async () => {
    const { live, gateway } = openLive({ maxFollows: 8, conversationSlots: 4 })
    try {
      const ref = 'agent/cursor-gap'
      await viewConversation(live, gateway, ref, snapshotRows(3))
      const feedAtom = live.source.conversation(ref)
      const syncAtom = live.source.sync!.conversation(ref)
      gateway.send({
        kind: 'resync', id: gateway.conversationSubscription(ref).id,
        collection: 'conversation', retryable: true, code: 'cursor-gap',
        message: 'The conversation cursor is no longer retained',
      })
      await until(() => live.registry.get(syncAtom).sync.status._tag === 'Stale')
      expect(live.registry.get(syncAtom).sync.status).toMatchObject({
        _tag: 'Stale',
        reason: { _tag: 'Resync', code: 'cursor-gap', message: 'The conversation cursor is no longer retained' },
      })
      expect(live.registry.get(feedAtom)).toMatchObject({ _tag: 'Observed', freshness: 'stale' })
      await vi.advanceTimersByTimeAsync(1000)
      await until(() => gateway.subscribesFor(ref) === 2)
      expect(live.registry.get(feedAtom)).toMatchObject({ _tag: 'Observed', freshness: 'stale' })
      gateway.conversationFrame(ref, [entry(100, 'Authoritative replacement')], true)
      await until(() => items(live.registry.get(feedAtom))?.[0]?.id === 'timeline-entry/100')
      expect(items(live.registry.get(feedAtom))?.map(item => item.id)).toEqual(['timeline-entry/100'])
      expect(live.registry.get(feedAtom)).toMatchObject({ _tag: 'Observed', freshness: 'live' })
      expect(live.registry.get(syncAtom).sync.status).toEqual({ _tag: 'Live', since: expect.any(Number) })
    } finally {
      await live.dispose()
    }
  })

  it('switches back to a retained live conversation without resubscribing', async () => {
    const { live, gateway } = openLive({ maxFollows: 8, conversationSlots: 4 })
    try {
      const closeWindows = mountWindows(live)
      await viewConversation(live, gateway, 'agent/warm', snapshotRows(5))
      expect(gateway.subscribesFor('agent/warm')).toBe(1)
      expect(gateway.unsubscribedIds()).toEqual([])

      const unmount = live.registry.mount(live.source.conversationInterest!('agent/warm'))
      const feed = live.registry.get(live.source.conversation('agent/warm'))
      expect(items(feed)!.length).toBe(5)
      expect(gateway.subscribesFor('agent/warm')).toBe(1)
      expect(gateway.unsubscribedIds()).toEqual([])
      unmount()
      closeWindows()
    } finally {
      await live.dispose()
    }
  })

  it('leaves no leaked subscriptions across churn and disposal', async () => {
    const { live, gateway } = openLive({ maxFollows: 8, conversationSlots: 3 })
    try {
      const closeWindows = mountWindows(live)
      for (let index = 0; index < 12; index += 1) {
        await viewConversation(live, gateway, `agent/churn-${index}`, snapshotRows(2))
        // At most one active subscription per conversation key, and never above the cap.
        const active = new Set<string>()
        for (const command of gateway.commands) {
          if (command.kind === 'subscribe') active.add(command.id)
          else if (command.kind === 'unsubscribe') active.delete(command.id)
        }
        const activeConversations = gateway.commands.filter(
          (command): command is Extract<CollectionCommand, { kind: 'subscribe'; collection: 'conversation' }> =>
            command.kind === 'subscribe' &&
            command.collection === 'conversation' &&
            active.has(command.id),
        )
        expect(new Set(activeConversations.map((command) => command.conversation)).size).toBe(
          activeConversations.length,
        )
        expect(active.size).toBeLessThanOrEqual(8)
        expect(getDebug('Wf.activeFollows')).toBeLessThanOrEqual(8)
      }
      // Every window mount is released and the source disposed: no follow survives.
      closeWindows()
      await live.dispose()
      for (let round = 0; round < 20; round += 1) await drain()
      expect(getDebug('Wf.activeFollows')).toBe(0)
    } finally {
      await live.dispose()
    }
  })

  it('ends the freshness consumer with its follow while the evicted feed stays retained', async () => {
    const { live, gateway } = openLive({ maxFollows: 8, conversationSlots: 2 })
    try {
      const closeWindows = mountWindows(live)
      await viewConversation(live, gateway, 'agent/doomed', snapshotRows(2))
      // The feed atom is still retained after stepping away; its follow is still admitted.
      const retained = live.registry.get(live.source.conversation('agent/doomed'))
      expect(items(retained)!.length).toBe(2)
      // Three window consumers plus this conversation's.
      expect(getDebug('Wf.freshnessConsumers')).toBe(4)
      // Viewing three more conversations (two slots) evicts the oldest with a real
      // unsubscribe: the follow's stream ends while its atom is still retained.
      for (const ref of ['agent/next-1', 'agent/next-2', 'agent/next-3'])
        await viewConversation(live, gateway, ref, snapshotRows(2))
      await until(() => gateway.unsubscribedIds().length >= 1)
      await drain()
      // The consumer belonged to the follow, not the atom: nothing stays subscribed to
      // the SDK's sync-status stream, and the evicted feed keeps its terminal verdict.
      expect(getDebug('Wf.freshnessConsumers')).toBe(5)
      const evicted = live.registry.get(live.source.sync!.conversation('agent/doomed'))
      expect(evicted.sync.status).toEqual({ _tag: 'Stale', reason: { _tag: 'Evicted' }, lastLiveAt: expect.any(Number) })
      // More churn on other keys writes nothing further into the evicted feed.
      const observedAt = evicted.sync.observedAt
      for (let index = 0; index < 3; index += 1)
        await viewConversation(live, gateway, `agent/after-${index}`, snapshotRows(2))
      expect(
        live.registry.get(live.source.sync!.conversation('agent/doomed')).sync.observedAt,
      ).toBe(observedAt)
      closeWindows()
    } finally {
      await live.dispose()
    }
  })

  it('derives the conversation slot budget from the advertised collections capability', async () => {
    // An older daemon advertises no collections capability: 8 slots, 4 for conversations.
    const legacy = openLive({ maxFollows: 8, conversationSlots: 'advertised' })
    try {
      const closeWindows = mountWindows(legacy.live)
      await until(() => getDebug('Wf.conversationSlots') === 4)
      expect(getDebug('Wf.followCap')).toBe(8)
      for (let index = 0; index < 4; index += 1)
        await viewConversation(legacy.live, legacy.gateway, `agent/legacy-${index}`, snapshotRows(1))
      expect(legacy.gateway.subscribesFor('agent/legacy-0')).toBe(1)
      await viewConversation(legacy.live, legacy.gateway, 'agent/legacy-4', snapshotRows(1))
      await until(() => legacy.gateway.unsubscribedIds().length >= 1)
      expect(legacy.gateway.subscribesFor('agent/legacy-0')).toBe(1)
      expect(legacy.gateway.unsubscribedIds().length).toBe(1)
      closeWindows()
    } finally {
      await legacy.live.dispose()
    }
    // The st main's collections v1 raises the cap to 16: 12 conversation slots.
    const modern = openLive({ maxFollows: 8, conversationSlots: 'advertised' })
    modern.gateway.collectionsV1 = true
    try {
      const closeWindows = mountWindows(modern.live)
      await until(() => getDebug('Wf.conversationSlots') === 12)
      expect(getDebug('Wf.followCap')).toBe(16)
      for (let index = 0; index < 12; index += 1)
        await viewConversation(modern.live, modern.gateway, `agent/modern-${index}`, snapshotRows(1))
      expect(modern.gateway.unsubscribedIds()).toEqual([])
      await viewConversation(modern.live, modern.gateway, 'agent/modern-12', snapshotRows(1))
      await until(() => modern.gateway.unsubscribedIds().length >= 1)
      expect(modern.gateway.subscribesFor('agent/modern-0')).toBe(1)
      closeWindows()
    } finally {
      await modern.live.dispose()
    }
  })
})
