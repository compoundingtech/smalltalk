import { it } from '@effect/vitest'
import { Context, Effect, Exit, Fiber, Layer, Option, Scope, Tracer } from 'effect'
import * as OtlpTracer from 'effect/observability/OtlpTracer'
import { afterEach, beforeEach, expect, vi } from 'vitest'

import { makeTelemetry, type Telemetry } from './browser.ts'

// Preserve the real scoped exporter while observing its factory through Vitest's ESM-safe spy.
vi.mock('effect/observability/OtlpTracer', { spy: true })

vi.mock('virtual:build-identity', () => ({
  buildIdentity: { machineVersion: 'test', sourceKind: 'git', dirty: false }, deploymentId: undefined,
}))

const endpoint = 'https://collector.example.test/v1/traces'
const frames = new Map<number, FrameRequestCallback>()
const painted: Array<() => void> = []
let nextFrame = 0
const payloads: OtlpTracer.TraceData[] = []
const fetch = vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
  payloads.push(JSON.parse(String(init?.body)))
  return new Response('', { status: 200 })
})

// Control both sides of afterNextPaint: rAF is still before paint; the posted task is after it.
class PaintChannel {
  private listener: (() => void) | undefined
  private closed = false
  readonly port1 = {
    addEventListener: (_type: string, listener: () => void) => { this.listener = listener },
    start: () => {},
    close: () => { this.closed = true },
  }
  readonly port2 = {
    postMessage: () => { painted.push(() => { if (!this.closed) this.listener?.() }) },
    close: () => { this.closed = true },
  }
}
const beforePaint = () => {
  const callbacks = [...frames.values()]
  frames.clear()
  for (const callback of callbacks) callback(performance.now())
}
const afterPaint = () => { for (const callback of painted.splice(0)) callback() }
const settle = Effect.promise(async () => {
  for (let round = 0; round < 10; round += 1) {
    await new Promise<void>((resolve) => setImmediate(resolve))
    await vi.advanceTimersByTimeAsync(0)
  }
})
const spans = () => payloads.flatMap((payload) => payload.resourceSpans.flatMap((resource) => resource.scopeSpans.flatMap((scope) => scope.spans)))
const make = () => makeTelemetry({ otlpTracesUrl: endpoint, resourceAttributes: {}, production: true })
const open = (telemetry: Telemetry) => Effect.gen(function* () {
  const scope = yield* Scope.make('sequential')
  yield* Effect.addFinalizer(() => Scope.close(scope, Exit.void))
  const context = yield* Layer.build(telemetry.layer).pipe(Scope.provide(scope))
  return { scope, tracer: Context.get(context, Tracer.Tracer) }
})
const firstFrame = (tracer: Tracer.Tracer, parent: Tracer.Span) => {
  const start = BigInt(Math.round(performance.timeOrigin * 1e6))
  const span = tracer.span({
    name: 'wf.ux.first_frame', parent: Option.none(), annotations: Context.empty(),
    links: [{ span: parent, attributes: { private: 'private-link-value' } }],
    startTime: start, kind: 'internal', root: true, sampled: true,
  })
  span.attribute('span.label', 'window')
  span.attribute('wf.subscription.kind', 'window')
  span.attribute('wf.ux.outcome', 'observed')
  span.attribute('wf.ux.budget_ms', 30_000)
  span.attribute('private', 'private-span-value')
  span.event('private-event', start, { private: 'private-event-value' })
  span.end(start + 20_000_000n, Exit.void)
  return span
}

beforeEach(() => {
  vi.mocked(OtlpTracer.make).mockClear()
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
  frames.clear()
  painted.length = 0
  payloads.length = 0
  fetch.mockClear()
  vi.stubGlobal('fetch', fetch)
  vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
    const id = ++nextFrame
    frames.set(id, callback)
    return id
  })
  vi.stubGlobal('cancelAnimationFrame', (id: number) => frames.delete(id))
  vi.stubGlobal('MessageChannel', PaintChannel)
})
afterEach(() => {
  vi.clearAllTimers()
  vi.useRealTimers()
  vi.unstubAllGlobals()
  vi.restoreAllMocks()
})

it.live('builds the synchronous tracer without creating an exporter, POST or export timer before paint', () => Effect.gen(function* () {
  const createTracer = vi.mocked(OtlpTracer.make)
  const timers = vi.spyOn(globalThis, 'setTimeout')
  const telemetry = make()
  const root = telemetry.ux.activeSpan()!
  expect(telemetry.ux.traceContext()?.traceparent).toBe(`00-${root.traceId}-${root.spanId}-01`)
  const { scope, tracer } = yield* open(telemetry)
  // Layer.build must finish without readiness: the real SDK can start and capture first frames.
  expect(tracer).toBeDefined()
  yield* settle
  expect(createTracer).not.toHaveBeenCalled()
  expect(timers.mock.calls.some(([, delay]) => delay === 1000)).toBe(false)
  yield* Effect.promise(() => vi.advanceTimersByTimeAsync(2000))
  expect(fetch).not.toHaveBeenCalled()
  telemetry.ux.shellCommitted()
  beforePaint()
  yield* settle
  expect(createTracer).not.toHaveBeenCalled()
  expect(fetch).not.toHaveBeenCalled()
  afterPaint()
  yield* settle
  expect(createTracer).toHaveBeenCalledOnce()
  expect(timers.mock.calls.some(([, delay]) => delay === 1000)).toBe(true)
  telemetry.ux.dispose()
  yield* Scope.close(scope, Exit.void)
}))

it.live('exports early first-frame roots and open reload roots with their original context, times and privacy filters', () => Effect.gen(function* () {
  const telemetry = make()
  const root = telemetry.ux.activeSpan()!
  // This transition happens before the telemetry layer is even built.
  telemetry.ux.observeSync({ key: {}, kind: 'window', status: { _tag: 'Live', since: 0 } })
  const { scope, tracer } = yield* open(telemetry)
  const first = firstFrame(tracer, root)
  const probe = tracer.span({
    name: 'st3.socket.probe', parent: Option.some(root), annotations: Context.empty(), links: [],
    startTime: first.status.startTime, kind: 'internal', root: false, sampled: true,
  })
  probe.attribute('span.label', 'private-sdk-label')
  probe.attribute('private', 'private-sdk-value')
  probe.end(first.status.startTime + 5_000_000n, Exit.fail('private-error-value'))
  const unknown = tracer.span({
    name: 'private-unknown-span', parent: Option.none(), annotations: Context.empty(), links: [],
    startTime: first.status.startTime, kind: 'internal', root: true, sampled: true,
  })
  unknown.end(first.status.startTime + 1n, Exit.void)
  yield* Effect.promise(() => vi.advanceTimersByTimeAsync(2000))
  expect(fetch).not.toHaveBeenCalled()
  expect(telemetry.ring.getSnapshot().spans.map((span) => span.name)).toContain('wf.ux.first_frame')
  telemetry.ux.shellCommitted()
  beforePaint()
  afterPaint()
  yield* settle
  // The root started before the exporter and ends after it: it must not disappear at cutover.
  telemetry.ux.rosterCommitted()
  beforePaint()
  afterPaint()
  yield* settle
  yield* Effect.promise(() => vi.advanceTimersByTimeAsync(1000))
  expect(spans().find((span) => span.name === first.name)).toMatchObject({
    traceId: first.traceId, spanId: first.spanId,
    startTimeUnixNano: String(first.status.startTime),
    endTimeUnixNano: String(first.status.startTime + 20_000_000n),
    links: [{ traceId: root.traceId, spanId: root.spanId, attributes: [] }],
  })
  expect(spans().find((span) => span.name === first.name)?.parentSpanId).toBeUndefined()
  expect(spans().find((span) => span.name === 'wf.ux.reload')).toMatchObject({ traceId: root.traceId, spanId: root.spanId })
  expect(spans().find((span) => span.name === 'st3.socket.probe')).toMatchObject({
    traceId: root.traceId, spanId: probe.spanId, parentSpanId: root.spanId,
    attributes: [{ key: 'span.label', value: { stringValue: 'socket.probe' } }],
    status: { code: 2 },
  })
  expect(spans().map((span) => span.name)).toContain('wf.sync.transition')
  expect(JSON.stringify(payloads)).not.toContain('private-')
  expect(payloads.every((payload) => payload.resourceSpans.every((resource) => resource.scopeSpans.every((scope) => scope.spans.length <= 16)))).toBe(true)
  expect(fetch.mock.calls.every(([request, init]) => String(request) === endpoint && init?.credentials === 'omit' && init.redirect === 'error' && init.keepalive === true)).toBe(true)
  telemetry.ux.dispose()
  yield* Scope.close(scope, Exit.void)
}))

it.live('replays bounded development events through the public event API for completed and still-open early spans', () => Effect.gen(function* () {
  const telemetry = makeTelemetry({ otlpTracesUrl: endpoint, resourceAttributes: {}, production: false })
  const { scope, tracer } = yield* open(telemetry)
  const startTime = BigInt(Math.round(performance.timeOrigin * 1e6))
  const completed = tracer.span({ name: 'dev.completed', parent: Option.none(), annotations: Context.empty(), links: [], startTime, kind: 'internal', root: true, sampled: true })
  for (let index = 0; index < 80; index += 1)
    completed.event('startup', startTime + BigInt(index), { index })
  completed.end(startTime + 100n, Exit.void)
  const active = tracer.span({ name: 'dev.active', parent: Option.none(), annotations: Context.empty(), links: [], startTime, kind: 'internal', root: true, sampled: true })
  active.event('before-ready', startTime, { phase: 'early' })
  telemetry.ux.shellCommitted()
  beforePaint()
  afterPaint()
  yield* settle
  active.event('after-ready', startTime + 200n, { phase: 'ready' })
  active.end(startTime + 300n, Exit.void)
  telemetry.ux.dispose()
  yield* Scope.close(scope, Exit.void)
  const completedExport = spans().find((span) => span.name === 'dev.completed')
  expect(completedExport).toMatchObject({ traceId: completed.traceId, spanId: completed.spanId })
  expect(completedExport?.events).toHaveLength(64)
  expect(completedExport?.events[0]).toMatchObject({ name: 'startup', timeUnixNano: String(startTime) })
  const activeExport = spans().find((span) => span.name === 'dev.active')
  expect(activeExport?.events.map((event) => event.name)).toEqual(['before-ready', 'after-ready'])
  expect(activeExport?.events[1]).toMatchObject({ timeUnixNano: String(startTime + 200n), attributes: [{ key: 'phase', value: { stringValue: 'ready' } }] })
}))

it.live('retains a bounded first-in startup buffer and drains it in bounded production batches', () => Effect.gen(function* () {
  const telemetry = make()
  const root = telemetry.ux.activeSpan()!
  const { scope, tracer } = yield* open(telemetry)
  const first = firstFrame(tracer, root)
  for (let index = 0; index < 300; index += 1) {
    const span = tracer.span({ name: 'st3.socket.probe', parent: Option.none(), annotations: Context.empty(), links: [], startTime: first.status.startTime, kind: 'internal', root: true, sampled: true })
    span.end(first.status.startTime + 1n, Exit.void)
  }
  expect(fetch).not.toHaveBeenCalled()
  telemetry.ux.shellCommitted()
  beforePaint()
  afterPaint()
  yield* settle
  yield* Effect.promise(() => vi.advanceTimersByTimeAsync(1000))
  expect(spans()).toHaveLength(256)
  expect(spans().some((span) => span.spanId === first.spanId)).toBe(true)
  expect(payloads.every((payload) => payload.resourceSpans.every((resource) => resource.scopeSpans.every((scope) => scope.spans.length <= 16)))).toBe(true)
  telemetry.ux.dispose()
  yield* Scope.close(scope, Exit.void)
}))

it.live('releases a bounded ten-second fallback when no meaningful paint occurs', () => Effect.gen(function* () {
  const createTracer = vi.mocked(OtlpTracer.make)
  const telemetry = make()
  const { scope, tracer } = yield* open(telemetry)
  const first = firstFrame(tracer, telemetry.ux.activeSpan()!)
  yield* settle
  yield* Effect.promise(() => vi.advanceTimersByTimeAsync(9999))
  expect(createTracer).not.toHaveBeenCalled()
  yield* Effect.promise(() => vi.advanceTimersByTimeAsync(1))
  yield* settle
  expect(createTracer).toHaveBeenCalledOnce()
  yield* Effect.promise(() => vi.advanceTimersByTimeAsync(1000))
  expect(spans().filter(span => span.spanId === first.spanId)).toHaveLength(1)
  yield* Scope.close(scope, Exit.void)
}))

for (const event of ['visible', 'error', 'unhandledrejection', 'pagehide'] as const) {
  it.live(`releases startup on ${event} without requiring a painted commit`, () => Effect.gen(function* () {
    vi.stubGlobal('window', new EventTarget())
    vi.stubGlobal('document', Object.assign(new EventTarget(), { visibilityState: 'visible' }))
    vi.stubGlobal('PerformanceObserver', class { static readonly supportedEntryTypes: string[] = [] })
    const createTracer = vi.mocked(OtlpTracer.make)
    const telemetry = make()
    const { scope, tracer } = yield* open(telemetry)
    const first = firstFrame(tracer, telemetry.ux.activeSpan()!)
    const uninstall = telemetry.install()
    if (event === 'visible') document.dispatchEvent(new Event('visibilitychange'))
    else window.dispatchEvent(new Event(event))
    yield* settle
    expect(createTracer).toHaveBeenCalledOnce()
    uninstall()
    yield* Scope.close(scope, Exit.void)
    expect(spans().filter(span => span.spanId === first.spanId)).toHaveLength(1)
    // Pagehide first drains completed startup spans; later shutdown ends the open reload root.
    expect(fetch).toHaveBeenCalledTimes(event === 'pagehide' ? 2 : 1)
  }))
}

it.live('exports buffered startup spans once when the page closes before readiness, including a queued paint', () => Effect.gen(function* () {
  const createTracer = vi.mocked(OtlpTracer.make)
  const telemetry = make()
  const root = telemetry.ux.activeSpan()!
  const { scope, tracer } = yield* open(telemetry)
  const first = firstFrame(tracer, root)
  telemetry.ux.shellCommitted()
  beforePaint()
  yield* Scope.close(scope, Exit.void)
  afterPaint()
  yield* settle
  yield* Effect.promise(() => vi.advanceTimersByTimeAsync(2000))
  expect(createTracer).toHaveBeenCalledOnce()
  expect(spans().filter(span => span.spanId === first.spanId)).toHaveLength(1)
  expect(fetch).toHaveBeenCalledOnce()
  telemetry.ux.dispose()
}))

it.live('waits for an in-flight pagehide flush and async body decode before completing scoped shutdown', () => Effect.gen(function* () {
  vi.stubGlobal('window', new EventTarget())
  vi.stubGlobal('document', Object.assign(new EventTarget(), { visibilityState: 'visible' }))
  vi.stubGlobal('PerformanceObserver', class { static readonly supportedEntryTypes: string[] = [] })
  let releaseDecode: (() => void) | undefined
  const decodeReady = new Promise<void>((resolve) => { releaseDecode = resolve })
  let requestStarted = false
  let bodyDecoded = false
  let signal: AbortSignal | undefined
  fetch.mockImplementationOnce(async (_input, init) => {
    requestStarted = true
    signal = init?.signal ?? undefined
    await decodeReady
    const body = await new Response(init?.body).text()
    payloads.push(JSON.parse(body))
    bodyDecoded = true
    return new Response('', { status: 200 })
  })
  const telemetry = make()
  const root = telemetry.ux.activeSpan()!
  const { scope, tracer } = yield* open(telemetry)
  yield* Effect.addFinalizer(() => Effect.sync(() => releaseDecode?.()))
  const uninstall = telemetry.install()
  yield* Effect.addFinalizer(() => Effect.sync(uninstall))
  const first = firstFrame(tracer, root)
  telemetry.ux.shellCommitted()
  beforePaint()
  afterPaint()
  yield* settle
  window.dispatchEvent(new Event('pagehide'))
  yield* settle
  expect(requestStarted).toBe(true)
  expect(bodyDecoded).toBe(false)
  uninstall()
  let closed = false
  const closing = yield* Effect.forkChild(Scope.close(scope, Exit.void).pipe(
    Effect.andThen(Effect.sync(() => { closed = true })),
  ))
  yield* settle
  expect(closed).toBe(false)
  expect(signal?.aborted).toBe(false)
  releaseDecode?.()
  yield* Fiber.join(closing)
  expect(closed).toBe(true)
  expect(bodyDecoded).toBe(true)
  expect(spans().find((span) => span.name === 'wf.ux.first_frame')).toMatchObject({ traceId: first.traceId, spanId: first.spanId })
  expect(spans().some((span) => span.name === 'wf.page.vitals')).toBe(true)
}))

it.live('shares one absolute second across stalled manual and native shutdown transport', () => Effect.gen(function* () {
  vi.stubGlobal('window', new EventTarget())
  vi.stubGlobal('document', Object.assign(new EventTarget(), { visibilityState: 'visible' }))
  vi.stubGlobal('PerformanceObserver', class { static readonly supportedEntryTypes: string[] = [] })
  let at = 0
  vi.spyOn(performance, 'now').mockImplementation(() => at)
  let releaseTransport: () => void = () => {}
  const stalled = new Promise<void>(resolve => { releaseTransport = resolve })
  const signals: AbortSignal[] = []
  const captureFetch = fetch.getMockImplementation()!
  fetch.mockImplementation(async (_input, init) => {
    if (init?.signal === undefined || init.signal === null) throw new Error('Missing transport cancellation signal')
    signals.push(init.signal)
    await stalled
    return new Response('', { status: 200 })
  })
  yield* Effect.addFinalizer(() => Effect.sync(releaseTransport))
  const telemetry = make()
  const { scope, tracer } = yield* open(telemetry)
  const root = telemetry.ux.activeSpan()!
  firstFrame(tracer, root)
  const uninstall = telemetry.install()
  yield* Effect.addFinalizer(() => Effect.sync(uninstall))
  telemetry.ux.shellCommitted()
  beforePaint()
  afterPaint()
  yield* settle
  window.dispatchEvent(new Event('pagehide'))
  yield* settle
  expect(signals).toHaveLength(1)
  // Leave another completed span in the native buffer while the manual request is stalled.
  firstFrame(tracer, root)
  let closed = false
  try {
    const closing = yield* Effect.forkChild(Scope.close(scope, Exit.void).pipe(
      Effect.andThen(Effect.sync(() => { closed = true })),
    ))
    yield* settle
    at = 999
    yield* Effect.promise(() => vi.advanceTimersByTimeAsync(999))
    expect(closed).toBe(false)
    at = 1000
    yield* Effect.promise(() => vi.advanceTimersByTimeAsync(1))
    yield* settle
    expect(closed).toBe(true)
    expect(signals.every(signal => signal.aborted)).toBe(true)
    yield* Fiber.join(closing)
  } finally {
    releaseTransport()
    fetch.mockImplementation(captureFetch)
  }
}))

it.live('honors early DOM shutdown even when the telemetry layer is constructed afterward', () => Effect.gen(function* () {
  vi.stubGlobal('window', new EventTarget())
  vi.stubGlobal('document', Object.assign(new EventTarget(), { visibilityState: 'visible' }))
  vi.stubGlobal('PerformanceObserver', class { static readonly supportedEntryTypes: string[] = [] })
  const createTracer = vi.mocked(OtlpTracer.make)
  const telemetry = make()
  const root = telemetry.ux.activeSpan()!
  const uninstall = telemetry.install()
  uninstall()
  const { scope } = yield* open(telemetry)
  yield* settle
  expect(createTracer).toHaveBeenCalledOnce()
  yield* Scope.close(scope, Exit.void)
  expect(spans().filter(span => span.spanId === root.spanId)).toHaveLength(1)
}))

it.live('flushes startup once when DOM telemetry is uninstalled before its pending shell paint', () => Effect.gen(function* () {
  vi.stubGlobal('window', new EventTarget())
  vi.stubGlobal('document', Object.assign(new EventTarget(), { visibilityState: 'visible' }))
  vi.stubGlobal('PerformanceObserver', class { static readonly supportedEntryTypes: string[] = [] })
  const createTracer = vi.mocked(OtlpTracer.make)
  const telemetry = make()
  const uninstall = telemetry.install()
  const { scope, tracer } = yield* open(telemetry)
  const first = firstFrame(tracer, telemetry.ux.activeSpan()!)
  telemetry.ux.shellCommitted()
  beforePaint()
  uninstall()
  afterPaint()
  yield* settle
  yield* Scope.close(scope, Exit.void)
  expect(createTracer).toHaveBeenCalledOnce()
  expect(spans().filter(span => span.spanId === first.spanId)).toHaveLength(1)
}))

it.live('shares one startup exporter when readiness races scoped shutdown', () => Effect.gen(function* () {
  const createTracer = vi.mocked(OtlpTracer.make)
  const telemetry = make()
  const { scope } = yield* open(telemetry)
  telemetry.ux.shellCommitted()
  beforePaint()
  afterPaint()
  yield* Scope.close(scope, Exit.void)
  yield* settle
  expect(createTracer).toHaveBeenCalledOnce()
  expect(fetch).toHaveBeenCalledOnce()
  telemetry.ux.dispose()
}))

it.live('starts once if shell paint predates layer construction, and ignores later roster paints', () => Effect.gen(function* () {
  const createTracer = vi.mocked(OtlpTracer.make)
  const telemetry = make()
  telemetry.ux.shellCommitted()
  beforePaint()
  afterPaint()
  expect(createTracer).not.toHaveBeenCalled()
  const { scope } = yield* open(telemetry)
  yield* settle
  expect(createTracer).toHaveBeenCalledOnce()
  telemetry.ux.rosterCommitted()
  beforePaint()
  afterPaint()
  yield* settle
  expect(createTracer).toHaveBeenCalledOnce()
  telemetry.ux.dispose()
  yield* Scope.close(scope, Exit.void)
}))
