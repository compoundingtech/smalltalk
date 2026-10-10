import { Context, Effect, Exit, Layer, ManagedRuntime, Option, Tracer } from 'effect'
import * as FetchHttpClient from 'effect/http/FetchHttpClient'
import type { TraceData } from 'effect/observability/OtlpTracer'

import { makeTelemetry } from './browser.ts'
import { afterNextPaint } from './ux.ts'

// Executable with the app's Vite config (which supplies virtual:build-identity). The browser
// runner asserts body[data-test-result=pass]; only the OTLP transport is replaced, not telemetry.
const prove = async () => {
  const earlyClose = new URLSearchParams(location.search).has('early-close')
  const payloads: TraceData[] = []
  const transportPolicies: Array<Pick<RequestInit, 'credentials' | 'redirect' | 'keepalive'>> = []
  let transportDecodeError: string | undefined
  let shellPainted = false
  let prematurePosts = 0
  let prematureExportTimers = 0
  const timer = globalThis.setTimeout
  globalThis.setTimeout = new Proxy(timer, {
    apply(target, receiver, args) {
      if (args[1] === 1000 && !shellPainted) prematureExportTimers += 1
      return Reflect.apply(target, receiver, args)
    },
  })
  const captureFetch: typeof globalThis.fetch = async (_input, init) => {
    transportPolicies.push({ credentials: init?.credentials, redirect: init?.redirect, keepalive: init?.keepalive })
    if (!shellPainted) prematurePosts += 1
    if (init?.credentials !== 'omit' || init.redirect !== 'error' || init.keepalive !== true)
      throw new Error('OTLP transport lost its privacy/keepalive policy')
    try {
      payloads.push(JSON.parse(await new Response(init.body).text()))
    } catch (error) {
      transportDecodeError = String(error)
      throw error
    }
    return new Response('', { status: 200 })
  }
  const telemetry = makeTelemetry({
    otlpTracesUrl: 'https://collector.example.test/v1/traces', production: true, resourceAttributes: {},
  })
  const root = telemetry.ux.activeSpan()!
  const uninstall = telemetry.install()
  const runtime = ManagedRuntime.make(telemetry.layer.pipe(
    Layer.provide(Layer.succeed(FetchHttpClient.Fetch, captureFetch)),
  ))
  try {
    // This must finish while there is no committed shell, rather than deadlocking SDK startup.
    await runtime.runPromise(Effect.void)
    const tracer = await runtime.runPromise(Tracer.Tracer)
    const startTime = BigInt(Math.round(performance.timeOrigin * 1e6))
    const first = tracer.span({
      name: 'wf.ux.first_frame', parent: Option.none(), annotations: Context.empty(),
      links: [{ span: root, attributes: { private: 'private-link-value' } }],
      startTime, kind: 'internal', root: true, sampled: true,
    })
    first.attribute('span.label', 'window')
    first.attribute('wf.subscription.kind', 'window')
    first.attribute('wf.ux.outcome', 'observed')
    first.attribute('private', 'private-span-value')
    first.end(startTime + 20_000_000n, Exit.void)
    if (payloads.length !== 0 || prematureExportTimers !== 0)
      throw new Error('OTLP started before a meaningful committed paint')

    if (earlyClose) {
      window.dispatchEvent(new PageTransitionEvent('pagehide', { persisted: false }))
      uninstall()
      await runtime.dispose()
    } else {
      const shell = document.createElement('main')
      shell.dataset.shell = 'committed'
      shell.textContent = 'Verified committed shell'
      document.getElementById('root')!.append(shell)
      // The marker is registered first at the same real browser paint boundary as telemetry.
      const painted = new Promise<void>((resolve) => afterNextPaint(() => {
        shellPainted = shell.isConnected && shell.textContent === 'Verified committed shell'
        resolve()
      }))
      telemetry.ux.shellCommitted()
      if (payloads.length !== 0 || prematureExportTimers !== 0)
        throw new Error('DOM commit alone incorrectly released OTLP before paint')
      await painted
      await new Promise<void>((resolve) => afterNextPaint(resolve))
      telemetry.ux.rosterCommitted()
      await new Promise<void>((resolve) => afterNextPaint(resolve))
      window.dispatchEvent(new PageTransitionEvent('pagehide', { persisted: true }))
      window.dispatchEvent(new PageTransitionEvent('pagehide', { persisted: true }))
      telemetry.ux.dispose()
      await runtime.dispose()
    }

    const spans = payloads.flatMap((payload) => payload.resourceSpans.flatMap((resource) => resource.scopeSpans.flatMap((scope) => scope.spans)))
    const firstExport = spans.find((span) => span.name === 'wf.ux.first_frame')
    const reloadExport = spans.find((span) => span.name === 'wf.ux.reload')
    if (!earlyClose && (prematurePosts !== 0 || prematureExportTimers !== 0 || !shellPainted))
      throw new Error('Exporter POST/timer preceded the actual committed shell paint')
    if (firstExport?.traceId !== first.traceId || firstExport.spanId !== first.spanId || firstExport.startTimeUnixNano !== String(startTime) || firstExport.endTimeUnixNano !== String(startTime + 20_000_000n))
      throw new Error(`Early first-frame identity/timing was lost at exporter startup: ${JSON.stringify({ actual: firstExport, payloadCount: payloads.length, transportPolicies, transportDecodeError, expected: { traceId: first.traceId, spanId: first.spanId, startTimeUnixNano: String(startTime), endTimeUnixNano: String(startTime + 20_000_000n) } })}`)
    if (firstExport.parentSpanId !== undefined || firstExport.links[0]?.spanId !== root.spanId)
      throw new Error('First-frame root/link topology was lost during replay')
    if (reloadExport?.spanId !== root.spanId || reloadExport.traceId !== root.traceId)
      throw new Error('Reload root opened before OTLP was not exported after readiness')
    if (spans.filter(span => span.spanId === first.spanId).length !== 1)
      throw new Error('Early startup span was exported more than once')
    if (spans.filter((span) => span.name === 'wf.page.vitals').length < (earlyClose ? 1 : 2))
      throw new Error('Pagehide summary was lost')
    if (JSON.stringify(payloads).includes('private-'))
      throw new Error('Early replay bypassed production privacy filters')
    return { closedBeforePaint: earlyClose, postsBeforePaint: prematurePosts, exportTimersBeforePaint: prematureExportTimers, shellPainted, firstFrameExported: true, reloadExported: true, pagehideSummaries: spans.filter((span) => span.name === 'wf.page.vitals').length }
  } finally {
    uninstall()
    await runtime.dispose()
    globalThis.setTimeout = timer
  }
}

try {
  document.body.dataset.proof = JSON.stringify(await prove())
  document.body.dataset.testResult = 'pass'
} catch (error) {
  document.body.dataset.proof = String(error)
  document.body.dataset.testResult = 'fail'
}
