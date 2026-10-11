/**
 * The real SDK under the real UX telemetry: a conversation switch's subscribe carries the
 * switch's trace (through the SDK's `st3.follow.subscribe` child), and the switch root completes
 * only after the frame is observed and the transcript paint is reported.
 */
import type { CollectionSocket, Snapshot } from '@smalltalk/st3-client'
import { FIRST_FRAME_SPAN, FOLLOW_SUBSCRIBE_SPAN, St3, St3Live } from '@st3/sdk/effect'
import * as Effect from 'effect/Effect'
import * as Option from 'effect/Option'
import * as Stream from 'effect/Stream'
import * as Tracer from 'effect/Tracer'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'

import { makeUxTelemetry } from './ux.ts'
import { makeSpanRing } from './spanRing.ts'

const API_VERSION = 'st3.client.v0'
const snapshot: Snapshot = {
  id: 'snapshot/1',
  created_at: '2026-10-08T00:00:00Z',
  host_id: 'host/build-a',
  projection_version: 'client-projection.v0',
  store_index: 1,
}

/** Drain socket callbacks, stream pulls and forked fibers (no wall-clock wait). */
const settle = Effect.promise(async () => {
  for (let round = 0; round < 10; round += 1) {
    await new Promise<void>((resolve) => setImmediate(resolve))
    await vi.advanceTimersByTimeAsync(0)
  }
})

beforeEach(() => vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] }))
afterEach(() => vi.useRealTimers())

it('a conversation switch traces subscribe -> first frame -> painted transcript under one root', async () => {
  const spans: Tracer.NativeSpan[] = []
  const ring = makeSpanRing()
  const tracer = Tracer.make({
    span: (options) => {
      const span = new Tracer.NativeSpan(options)
      spans.push(span)
      const end = span.end.bind(span)
      span.end = (endTime, exit) => {
        end(endTime, exit)
        ring.push({
          name: span.name,
          label: String(span.attributes.get('span.label') ?? ''),
          startMs: Number(options.startTime) / 1e6 - performance.timeOrigin,
          durationMs: Number(endTime - options.startTime) / 1e6,
          attributes: Object.fromEntries(span.attributes),
        })
      }
      return span
    },
  })
  const paints: Array<() => void> = []
  const ux = makeUxTelemetry({
    tracer: () => tracer,
    paint: (callback) => {
      paints.push(callback)
      return () => {}
    },
  })
  const reloadRoot = ux.activeSpan()!
  const commands: Array<Record<string, unknown>> = []
  let socket: CollectionSocket | undefined
  const headers: Array<string | null> = []
  const ref = 'session/switch-proof'

  await Effect.gen(function* () {
    const st3 = yield* St3
    yield* settle
    ux.beginSwitch({ ref, warm: false, slotCount: 1 })
    const switchRoot = ux.activeSpan()!
    let observed = 0
    yield* Effect.forkChild(
      st3.followConversation({ _tag: 'Conversation', ref }).pipe(
        Stream.runForEach((event) =>
          Effect.sync(() => {
            if (event._tag !== 'Observed') return
            observed += 1
            ux.switchDataReady(ref)
          }),
        ),
      ),
    )
    yield* settle
    const subscribe = commands.find((command) => command['kind'] === 'subscribe' && command['collection'] === 'conversation')!
    const child = spans.find((span) => span.name === FOLLOW_SUBSCRIBE_SPAN)!
    expect(Option.getOrUndefined(child.parent)).toBe(switchRoot)
    // Probe HTTP also names its own SDK child, and keeps context even without an active UX root.
    const probe = spans.find((span) => span.name === 'st3.socket.probe')!
    expect(Option.getOrUndefined(probe.parent)).toBe(reloadRoot)
    expect(headers[0]).toBe(`00-${reloadRoot.traceId}-${probe.spanId}-01`)
    // The subscribe frame names the SDK's own child span, not the switch root.
    expect(subscribe['trace']).toEqual({ traceparent: `00-${switchRoot.traceId}-${child.spanId}-01` })
    // So does an SDK HTTP read issued while the switch is active.
    yield* st3.snapshot
    const read = spans.find((span) => span.name === 'st3.snapshot')!
    expect(Option.getOrUndefined(read.parent)).toBe(switchRoot)
    expect(headers.at(-1)).toBe(`00-${switchRoot.traceId}-${read.spanId}-01`)

    socket?.onmessage?.({
      data: JSON.stringify({
        kind: 'conversation',
        id: subscribe['id'],
        collection: 'conversation',
        session_id: ref,
        items: [],
        replace: true,
        has_more: false,
      }),
    })
    yield* settle
    expect(observed).toBe(1)
    ux.transcriptCommitted(ref)
    for (const paint of paints.splice(0)) paint()

    yield* Effect.promise(() => vi.advanceTimersByTimeAsync(250))
    expect(ring.getSnapshot().spans.map((span) => span.name)).toEqual(expect.arrayContaining([
      'wf.ux.switch', 'wf.ux.switch.data_ready', 'st3.snapshot', FOLLOW_SUBSCRIBE_SPAN, FIRST_FRAME_SPAN,
    ]))
    const root = spans.find((span) => span === switchRoot)!
    expect(root.status._tag).toBe('Ended')
    expect(root.attributes.get('wf.ux.outcome')).toBe('painted')
    expect(spans.find((span) => span.name === 'wf.ux.switch.data_ready')?.attributes.get('wf.ux.outcome')).toBe('observed')
    expect(child.status._tag).toBe('Ended')
    expect(child.attributes.get('wf.ux.outcome')).toBe('observed')
    expect(child.attributes.get('wf.subscription.kind')).toBe('conversation')
    const firstFrame = spans.find((span) => span.name === FIRST_FRAME_SPAN && span.attributes.get('wf.subscription.kind') === 'conversation')!
    expect(firstFrame.parent._tag).toBe('None')
    expect(firstFrame.attributes.get('wf.ux.outcome')).toBe('observed')
    expect(firstFrame.attributes.get('wf.ux.budget_ms')).toBe(30_000)
    expect(firstFrame.attributes.get('wf.ux.budget_phase')).toBe('subscribe-to-first-frame')
    expect(firstFrame.attributes.get('wf.ux.budget_met')).toBe(true)
    // Bounded labels only: the ref never becomes an attribute value.
    for (const span of [root, child, firstFrame])
      expect([...span.attributes.values()].some((value) => String(value).includes(ref))).toBe(false)
  }).pipe(
    Effect.provide(
      St3Live({
        baseUrl: 'http://gateway.test',
        maxFollows: 4,
        traceContext: ux.traceContext,
        parentSpan: ux.activeSpan,
        fetch: async (_input, init) => {
          headers.push(new Headers(init?.headers).get('traceparent'))
          return new Response(JSON.stringify({ api_version: API_VERSION, snapshot, value: {} }))
        },
        socket: () => {
          const opened: CollectionSocket = {
            onopen: null,
            onmessage: null,
            onclose: null,
            onerror: null,
            send: (text) => commands.push(JSON.parse(text)),
            close: () => {},
          }
          socket = opened
          queueMicrotask(() => opened.onopen?.())
          return opened
        },
      }),
    ),
    Effect.scoped,
    Effect.withTracer(tracer),
    Effect.runPromise,
  )
  ux.dispose()
})
