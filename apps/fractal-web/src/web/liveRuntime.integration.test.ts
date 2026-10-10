import { Effect } from 'effect'
import * as FetchHttpClient from 'effect/http/FetchHttpClient'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'

// Isolate the page's transport ownership; use the real telemetry and OTLP exporter.
vi.mock('../data/liveSource.ts', () => ({ liveSource: () => ({}) }))
vi.mock('virtual:build-identity', () => ({
  buildIdentity: { machineVersion: 'test' }, deploymentId: undefined,
}))

const frames: FrameRequestCallback[] = []
beforeEach(() => {
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
  vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => { frames.push(callback); return frames.length })
  vi.stubGlobal('cancelAnimationFrame', () => {})
})

afterEach(() => {
  frames.length = 0
  vi.clearAllTimers()
  vi.useRealTimers()
  vi.unstubAllGlobals()
  vi.unstubAllEnvs()
  vi.restoreAllMocks()
  vi.resetModules()
})

it('captures the mandatory reload root synchronously at page runtime construction, before SDK ownership', async () => {
  vi.stubGlobal('window', { location: { origin: 'https://example.test' } })
  // Static import would construct the page entry before the browser globals are installed.
  const { createPageRuntime } = await import('./liveRuntime.ts')
  const timers = vi.spyOn(globalThis, 'setTimeout')
  const { telemetry } = createPageRuntime()
  // Inspect before accessing the lazy getter: runtime construction itself must have captured it.
  expect(timers.mock.calls.filter(([, delay]) => delay === 30_000)).toHaveLength(1)
  const root = telemetry.ux.activeSpan()!
  expect(root.name).toBe('wf.ux.reload')
  expect(telemetry.ux.traceContext()?.traceparent).toBe(`00-${root.traceId}-${root.spanId}-01`)
  telemetry.ux.dispose()
})

const exportPageSpan = async (endpoint: string | undefined) => {
  vi.stubEnv('VITE_OTLP_TRACES_URL', endpoint)
  vi.stubGlobal('window', { location: { origin: 'https://example.test' } })
  const fetch = vi.fn(async (_input: RequestInfo | URL, _init?: RequestInit) => new Response('', { status: 200 }))
  const sendBeacon = vi.fn(() => true)
  vi.stubGlobal('fetch', fetch)
  vi.stubGlobal('navigator', { sendBeacon })
  const errors = vi.spyOn(console, 'error').mockImplementation(() => {})
  // Import after configuring the browser: this test exercises the page module loading boundary.
  const { createPageRuntime } = await import('./liveRuntime.ts')
  const { telemetry } = createPageRuntime()
  // Layer construction captures immediately; only the committed shell's after-paint task starts
  // the exporter. Closing after that task still exercises its deterministic final flush.
  await Effect.runPromise(Effect.gen(function* () {
    yield* Effect.void.pipe(Effect.withSpan('wf.ux.reload'))
    expect(fetch).not.toHaveBeenCalled()
    telemetry.ux.shellCommitted()
    for (const frame of frames.splice(0)) frame(performance.now())
    yield* Effect.promise(async () => {
      for (let round = 0; round < 10; round += 1)
        await new Promise<void>((resolve) => setImmediate(resolve))
    })
  }).pipe(
    Effect.provide(telemetry.layer),
    Effect.provideService(FetchHttpClient.Fetch, fetch),
    Effect.scoped,
  ))
  telemetry.ux.dispose()
  return { fetch, sendBeacon, errors }
}

it('does not install a browser exporter without an explicitly configured endpoint', async () => {
  const { fetch, sendBeacon, errors } = await exportPageSpan(undefined)
  expect(fetch).not.toHaveBeenCalled()
  expect(sendBeacon).not.toHaveBeenCalled()
  expect(errors).not.toHaveBeenCalled()
})

it('exports to the explicitly configured traces endpoint', async () => {
  const endpoint = 'https://collector.example.test/v1/traces'
  const { fetch, sendBeacon, errors } = await exportPageSpan(endpoint)
  expect(fetch).toHaveBeenCalled()
  expect(fetch.mock.calls.every(([request]) => String(request) === endpoint)).toBe(true)
  expect(sendBeacon).not.toHaveBeenCalled()
  expect(errors).not.toHaveBeenCalled()
})
