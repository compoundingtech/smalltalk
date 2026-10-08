/**
 * wf browser telemetry (`service.name = webfractal-web`).
 *
 * One Effect `Tracer` serves both kinds of spans:
 * - Effect spans (SDK `st3.*`, app `wf.*` effects) flow through it as usual.
 * - Browser measurements become retroactive spans with explicit start/end times:
 *   `wf.ui.interaction` (input event → first React commit → next paint, INP-like, for every
 *   pointerdown/keydown including fast ones Event Timing does not report), `wf.main.long_frame`
 *   (Long Animation Frames ≥ 50 ms with blocking time and top script), `wf.page.vitals`
 *   (INP / LCP / CLS summary on pagehide).
 *
 * The tracer is teed into an in-memory `SpanRing` for the dev perf panel and, when an OTLP URL is
 * given, exported as OTLP/JSON via `fetch` directly to that explicitly configured endpoint.
 */
import { Context, Deferred, Effect, Exit, FiberSet, Layer, Option, Scope, Tracer } from 'effect'
import * as FetchHttpClient from 'effect/http/FetchHttpClient'
import * as OtlpExporter from 'effect/observability/OtlpExporter'
import * as OtlpSerialization from 'effect/observability/OtlpSerialization'
import * as OtlpTracer from 'effect/observability/OtlpTracer'
import type React from 'react'
import { buildIdentity, deploymentId } from 'virtual:build-identity'

import { makeSpanRing, type SpanRing } from './spanRing.ts'
import { afterNextPaint, makeUxTelemetry, uxAttributeNames, uxSpanNames, type UxTelemetry } from './ux.ts'

/** OpenTelemetry resource identity for browser spans emitted by wf. */
export const serviceName = 'webfractal-web'

/**
 * Event types that start a user interaction. React Aria selects on press start (pointerdown) for
 * mouse and on keydown for keyboard, so `click` would arrive after the commit.
 */
const interactionEvents = ['pointerdown', 'keydown'] as const

/** Interaction spans end at most this long after the input (the latest paint seen by then). */
const settleMs = 500

/** Configures where browser spans go and which bounded resource dimensions they carry. */
export interface TelemetryOptions {
  /** Absolute OTLP/HTTP traces URL; undefined keeps spans in-process (ring only). */
  readonly otlpTracesUrl: string | undefined
  /** Low-cardinality resource attributes, e.g. `{ 'wf.lab.contender': 'atom' }`. */
  readonly resourceAttributes: Readonly<Record<string, string | number>>
  /** Production omits script names and strips SDK events, failure details and unknown spans. */
  readonly production?: boolean
}

/** One browser telemetry instance: retained span ring, Effect tracer layer, profiler hook and DOM installation. */
export interface Telemetry {
  readonly ring: SpanRing
  readonly ux: UxTelemetry
  /** Provides the synchronous teed `Tracer`; OTLP starts only after a shell/roster paint. */
  readonly layer: Layer.Layer<never>
  /** Root `React.Profiler` callback: marks commits so interactions can measure commit latency. */
  readonly onCommit: React.ProfilerOnRenderCallback
  /** Installs event listeners and PerformanceObservers; returns the uninstaller. */
  readonly install: () => () => void
}

interface PendingInteraction {
  readonly type: string
  readonly target: string
  readonly start: number
  commits: number
  commitAt: number | undefined
  /** Paint after the first observed React commit (Profiler; dev/profiling builds only). */
  commitPaintAt: number | undefined
  /** Paint after the input event itself; used when no commit was observed. */
  eventPaintAt: number | undefined
}

interface EarlySpanCapture {
  readonly span: Tracer.Span
  readonly events: ReadonlyArray<Parameters<Tracer.Span['event']>> | undefined
}

// Never export free-form data-perf-target, DOM IDs, selected refs or accessible text.
const interactionTargets: Readonly<Record<string, true>> = {
  'agents-list': true,
  'conversation-list': true,
  tabs: true,
  palette: true,
  'missions-list': true,
  'perf-controls': true,
}
const interactionTarget = (target: EventTarget | null): string => {
  if (!(target instanceof Element)) return 'other'
  const named = target.closest('[data-perf-target]')?.getAttribute('data-perf-target')
  if (named !== undefined && named !== null)
    return Object.hasOwn(interactionTargets, named) ? named : 'other'
  // Structural production regions: only constant outputs, never read the identifier/text.
  if (target.closest('[role="dialog"][aria-label="Command palette"]')) return 'palette'
  if (target.closest('[role="tab"],[role="tablist"]')) return 'tabs'
  if (target.closest('[role="treegrid"]')) return 'agents-list'
  if (
    target.closest(
      '[data-conversation-entry-id],[role="region"][aria-label="Conversation history"]',
    )
  )
    return 'conversation-list'
  return 'other'
}

const exportedSpanNames: Readonly<Record<string, true>> = {
  ...Object.fromEntries(uxSpanNames.map((name) => [name, true as const])),
  'st3.follow.subscribe': true,
  'st3.capabilities': true,
  'st3.socket.probe': true,
  'st3.socket.create': true,
  'wf.ui.interaction': true,
  'wf.main.long_frame': true,
  'wf.page.vitals': true,
  'st3.snapshot': true,
  'st3.messageSend': true,
}
const exportedAttributes: Readonly<Record<string, true>> = {
  ...Object.fromEntries(uxAttributeNames.map((name) => [name, true as const])),
  'span.label': true,
  'wf.ux.early': true,
  'wf.interaction.type': true,
  'wf.interaction.target': true,
  'wf.interaction.commit_observed': true,
  'wf.interaction.commits': true,
  'wf.interaction.commit_ms': true,
  'wf.frame.blocking_ms': true,
  'wf.frame.render_ms': true,
  'wf.vitals.cls': true,
  'wf.vitals.long_frames': true,
  'wf.vitals.inp_ms': true,
  'wf.vitals.lcp_ms': true,
}

/** Long Animation Frames API (Chromium); not in TypeScript's DOM lib yet. */
interface LongAnimationFrameEntry extends PerformanceEntry {
  readonly blockingDuration: number
  readonly renderStart: number
  readonly scripts: ReadonlyArray<{
    readonly invoker: string
    readonly duration: number
    readonly sourceFunctionName: string
  }>
}

interface EventTimingEntry extends PerformanceEntry {
  readonly interactionId: number
  readonly processingStart: number
  readonly processingEnd: number
}

interface LayoutShiftEntry extends PerformanceEntry {
  readonly value: number
  readonly hadRecentInput: boolean
}

const toNanos = (performanceMs: number) =>
  BigInt(Math.round((performance.timeOrigin + performanceMs) * 1_000_000))


const supportsEntryType = (type: string) => PerformanceObserver.supportedEntryTypes.includes(type)

/** Builds teed browser telemetry without touching DOM observers until `install` runs. */
export const makeTelemetry = (options: TelemetryOptions): Telemetry => {
  const { otlpTracesUrl } = options
  const ring = makeSpanRing()
  const ready = Deferred.makeUnsafe<void>()
  // Keep the first startup spans, rather than a rolling tail that could evict first-frame roots.
  const earlySpans: EarlySpanCapture[] = []
  const earlySpanLimit = 256
  let exporter: Tracer.Tracer | undefined
  let disposed = false
  let flush: (() => void) | undefined
  let flushRequested = false
  let shutdownRequested = false
  let closeExporter: (() => void) | undefined

  /** Replay only pre-exporter spans, retaining the IDs already propagated to the SDK/gateway. */
  const exportEarlySpan = ({ span, events }: EarlySpanCapture, target: Tracer.Tracer) => {
    if (span.status._tag !== 'Ended') return
    const replay = target.span({
      name: span.name,
      parent: span.parent,
      annotations: span.annotations,
      links: Array.from(span.links),
      startTime: span.status.startTime,
      kind: span.kind,
      root: Option.isNone(span.parent),
      sampled: span.sampled,
    })
    // OTLP generates IDs lazily. Fix the public identity before its end/serialization, without
    // depending on Effect's private span fields or re-parenting independent first-frame roots.
    Object.defineProperties(replay, {
      traceId: { value: span.traceId },
      spanId: { value: span.spanId },
    })
    for (const [key, value] of span.attributes) replay.attribute(key, value)
    if (events !== undefined)
      for (const [name, at, attributes] of events) replay.event(name, at, attributes)
    replay.end(span.status.endTime, span.status.exit)
  }

  // This tracer is available before Layer.build and never changes identity. Spans that start
  // before readiness still end into the eventual exporter, even if they remain open at paint.
  const deferredTracer = Tracer.make({
    span(spanOptions) {
      if (otlpTracesUrl === undefined || disposed || !spanOptions.sampled)
        return Tracer.nativeTracer.span(spanOptions)
      if (exporter !== undefined) return exporter.span(spanOptions)
      const span = Tracer.nativeTracer.span(spanOptions)
      // Tracer.Span exposes event(), not an events read model. Capture through that public API;
      // production replaces it with a no-op in the tee, so it needs no event buffer at all.
      const events: Array<Parameters<Tracer.Span['event']>> | undefined = options.production ? undefined : []
      if (events !== undefined) {
        span.event = (...event) => {
          if (events.length < 64) events.push(event)
        }
      }
      const captured: EarlySpanCapture = { span, events }
      const end = span.end.bind(span)
      span.end = (at, exit) => {
        end(at, exit)
        if (disposed) return
        if (exporter !== undefined) exportEarlySpan(captured, exporter)
        else if (earlySpans.length < earlySpanLimit) earlySpans.push(captured)
      }
      return span
    },
  })

  /** Wraps a tracer so every ended span is also pushed into the ring. */
  const tee = (inner: Tracer.Tracer): Tracer.Tracer =>
    Tracer.make({
      span(spanOptions) {
        const span =
          options.production && !Object.hasOwn(exportedSpanNames, spanOptions.name)
            ? Tracer.nativeTracer.span(spanOptions)
            : inner.span(options.production ? {
                ...spanOptions,
                // First-frame roots link the early bootstrap, but never export link attributes/baggage.
                links: spanOptions.name === 'wf.ux.first_frame' ? spanOptions.links.map(({ span }) => ({
                  span: Tracer.externalSpan({ traceId: span.traceId, spanId: span.spanId, sampled: span.sampled }),
                  attributes: {},
                })) : [],
              } : spanOptions)
        if (options.production) {
          const attribute = span.attribute.bind(span)
          // SDK labels are fixed operation names, never a resource ref supplied by the caller.
          if (span.name.startsWith('st3.') && Object.hasOwn(exportedSpanNames, span.name))
            attribute('span.label', span.name.slice(4))
          Object.assign(span, {
            attribute: (key: string, value: unknown) => {
              if (Object.hasOwn(exportedAttributes, key) && !span.name.startsWith('st3.'))
                attribute(key, value)
            },
            event: () => {},
            addLinks: () => {},
          })
        }
        const end = span.end.bind(span)
        Object.assign(span, {
          end: (endTime: bigint, exit: Exit.Exit<unknown, unknown>) => {
            // OTLP serializes failures into exception messages/stacks. Preserve failure status,
            // but never hand it the SDK's response error (which can contain bodies or identities).
            end(endTime, options.production && Exit.isFailure(exit) ? Exit.fail(undefined) : exit)
            const startMs = Number(spanOptions.startTime / 1_000n) / 1_000 - performance.timeOrigin
            ring.push({
              name: span.name,
              label: String(span.attributes.get('span.label') ?? ''),
              startMs,
              durationMs: Number((endTime - spanOptions.startTime) / 1_000n) / 1_000,
              attributes: Object.fromEntries(span.attributes),
            })
          },
        })
        return span
      },
      ...(inner.context === undefined ? {} : { context: inner.context }),
    })
  const tracer = tee(deferredTracer)
  let ux: UxTelemetry | undefined
  const getUx = () => ux ??= makeUxTelemetry({
    tracer: () => tracer,
    onFirstPaint: () => {
      if (!disposed) Deferred.doneUnsafe(ready, Effect.void)
    },
  })

  const resource = {
    serviceName,
    serviceVersion: buildIdentity.machineVersion,
    attributes: {
      ...options.resourceAttributes,
      'build.source_kind': buildIdentity.sourceKind,
      'build.dirty': buildIdentity.dirty,
      ...(buildIdentity.rev === undefined ? {} : { 'build.rev': buildIdentity.rev }),
      ...(deploymentId === undefined ? {} : { 'deployment.id': deploymentId }),
    },
  }

  const layer: Layer.Layer<never> =
    otlpTracesUrl === undefined
      ? Layer.succeed(Tracer.Tracer, tracer)
      : Layer.effect(
          Tracer.Tracer,
          Effect.gen(function* () {
            const scope = yield* Scope.Scope
            const exporterScope = yield* Scope.make()
            let shutdownDeadline = 0
            // Effect reads a DurationObject when its sleep executes, not when the exporter is
            // constructed. Both sequential finalizers therefore consume the same remaining time.
            const shutdownTimeout = {
              get milliseconds() { return Math.max(0, shutdownDeadline - performance.now()) },
            }
            const initialize = yield* Effect.cached(Effect.gen(function* () {
              const otlp = yield* OtlpTracer.make({
                url: otlpTracesUrl,
                resource,
                exportInterval: '1 second',
                shutdownTimeout,
                maxBatchSize: options.production ? 16 : 128,
              })
              const flusher = yield* OtlpExporter.Flusher
              const fibers = yield* FiberSet.make<void, never>()
              yield* Effect.addFinalizer(() =>
                FiberSet.awaitEmpty(fibers).pipe(Effect.timeoutOption(shutdownTimeout), Effect.asVoid),
              )
              const run = yield* FiberSet.runtime(fibers)<never>()
              exporter = otlp
              flush = () => { run(flusher.flush.pipe(Effect.timeoutOption('1 second'))) }
              for (const span of earlySpans) exportEarlySpan(span, otlp)
              earlySpans.length = 0
              if (flushRequested) flush()
            }).pipe(
              Effect.provideService(Scope.Scope, exporterScope),
              Effect.uninterruptible,
            ))
            const shutdown = yield* Effect.cached(Effect.gen(function* () {
              // Initialization, manual drain and native final flush share one absolute deadline.
              shutdownDeadline = performance.now() + 1000
              yield* initialize
              yield* Scope.close(exporterScope, Exit.void)
              flush = undefined
              exporter = undefined
              closeExporter = undefined
              earlySpans.length = 0
            }).pipe(Effect.uninterruptible))
            const services = yield* Effect.context<Effect.Services<typeof shutdown>>()
            closeExporter = () => { Effect.runForkWith(services)(shutdown) }
            // Normal startup remains paint-gated. Hidden tabs and failed startup still get a
            // bounded fallback; teardown shares this initializer instead of dropping its buffer.
            yield* Effect.raceFirst(Deferred.await(ready), Effect.sleep('10 seconds')).pipe(
              Effect.andThen(Effect.suspend(() => shutdownRequested ? shutdown : initialize)),
              Effect.forkIn(scope),
            )
            yield* Effect.addFinalizer(() => {
              ux?.dispose()
              disposed = true
              shutdownRequested = true
              return shutdown
            })
            return tracer
          }),
        ).pipe(
          Layer.provide(OtlpExporter.layerFlusher),
          Layer.provide(OtlpSerialization.layerJson),
          Layer.provide(
            FetchHttpClient.layer.pipe(
              Layer.provide(
                Layer.succeed(FetchHttpClient.RequestInit, {
                  credentials: 'omit',
                  redirect: 'error',
                  keepalive: options.production ?? false,
                }),
              ),
            ),
          ),
        )

  /** Records a span that already happened (browser-measured start and end). */
  const emit = ({
    name,
    startMs,
    endMs,
    attributes,
  }: {
    readonly name: string
    readonly startMs: number
    readonly endMs: number
    readonly attributes: Readonly<Record<string, unknown>>
  }) => {
    const span = tracer.span({
      name,
      parent: Option.none(),
      annotations: Context.empty(),
      links: [],
      startTime: toNanos(startMs),
      kind: 'internal',
      root: true,
      sampled: true,
    })
    for (const [key, value] of Object.entries(attributes)) span.attribute(key, value)
    span.end(toNanos(endMs), Exit.void)
  }

  const pending: PendingInteraction[] = []
  const timers = new Map<PendingInteraction, number>()
  const paints = new Set<() => void>()
  const afterPaint = (f: () => void) => {
    const cancel = afterNextPaint(() => {
      paints.delete(cancel)
      f()
    })
    paints.add(cancel)
  }
  let uninstall: (() => void) | undefined

  /**
   * Ends the span `settleMs` after the input. End = the paint after the first commit when the
   * Profiler saw one (dev/profiling builds), else the paint after the event itself: production
   * React strips `<Profiler>`, and a discrete-event update commits inside the event dispatch, so
   * the event's next paint already contains it.
   */
  const finish = (interaction: PendingInteraction) => {
    const index = pending.indexOf(interaction)
    if (index === -1) return
    pending.splice(index, 1)
    timers.delete(interaction)
    const paintAt = interaction.commitPaintAt ?? interaction.eventPaintAt
    const commitMs =
      interaction.commitAt === undefined ? undefined : interaction.commitAt - interaction.start
    emit({
      name: 'wf.ui.interaction',
      startMs: interaction.start,
      endMs: paintAt ?? interaction.start + settleMs,
      attributes: {
        'span.label': `${interaction.type} ${interaction.target}`,
        'wf.interaction.type': interaction.type,
        'wf.interaction.target': interaction.target,
        'wf.interaction.commit_observed': interaction.commitAt !== undefined,
        'wf.interaction.commits': interaction.commits,
        ...(commitMs === undefined ? {} : { 'wf.interaction.commit_ms': commitMs }),
      },
    })
  }

  const onCommit: React.ProfilerOnRenderCallback = () => {
    if (pending.length === 0) return
    const now = performance.now()
    for (const interaction of pending) {
      interaction.commits += 1
      if (interaction.commitAt === undefined) {
        interaction.commitAt = now
        afterPaint(() => {
          interaction.commitPaintAt = performance.now()
        })
      }
    }
  }

  const onInteraction = (event: Event) => {
    if (event instanceof KeyboardEvent && (event.repeat || event.key.length > 1)) {
      if (event.key !== 'Enter' && event.key !== 'Backspace') return
    }
    const target = interactionTarget(event.target)
    const interaction: PendingInteraction = {
      type: event.type,
      target,
      start: event.timeStamp,
      commits: 0,
      commitAt: undefined,
      commitPaintAt: undefined,
      eventPaintAt: undefined,
    }
    pending.push(interaction)
    afterPaint(() => {
      interaction.eventPaintAt = performance.now()
    })
    timers.set(
      interaction,
      window.setTimeout(() => finish(interaction), settleMs),
    )
  }

  const install = () => {
    if (uninstall !== undefined) return uninstall
    for (const type of interactionEvents)
      window.addEventListener(type, onInteraction, { capture: true })

    const observers: PerformanceObserver[] = []
    const observe = ({
      type,
      init,
      handle,
    }: {
      readonly type: string
      readonly init: PerformanceObserverInit
      readonly handle: (entries: ReadonlyArray<PerformanceEntry>) => void
    }) => {
      if (!supportsEntryType(type)) return
      const observer = new PerformanceObserver((list) => handle(list.getEntries()))
      observer.observe({ type, buffered: true, ...init })
      observers.push(observer)
    }

    let inpMs: number | undefined
    observe({
      type: 'event',
      init: { durationThreshold: 16 } as PerformanceObserverInit,
      handle: (entries) => {
        for (const entry of entries as ReadonlyArray<EventTimingEntry>) {
          if (entry.interactionId > 0 && (inpMs === undefined || entry.duration > inpMs)) {
            inpMs = entry.duration
            ring.updateVitals({ inpMs })
          }
        }
      },
    })

    let longFrames = 0
    let longFrameMaxMs = 0
    observe({
      type: 'long-animation-frame',
      init: {},
      handle: (entries) => {
        for (const entry of entries as ReadonlyArray<LongAnimationFrameEntry>) {
          longFrames += 1
          longFrameMaxMs = Math.max(longFrameMaxMs, entry.duration)
          const top = options.production
            ? undefined
            : entry.scripts.toSorted((a, b) => b.duration - a.duration)[0]
          emit({
            name: 'wf.main.long_frame',
            startMs: entry.startTime,
            endMs: entry.startTime + entry.duration,
            attributes: {
              'span.label': 'long frame',
              'wf.frame.blocking_ms': entry.blockingDuration,
              'wf.frame.render_ms': entry.startTime + entry.duration - entry.renderStart,
              ...(top === undefined
                ? {}
                : { 'wf.frame.top_script': (top.sourceFunctionName || top.invoker).slice(0, 80) }),
            },
          })
        }
        ring.updateVitals({ longFrames, longFrameMaxMs })
      },
    })

    observe({
      type: 'largest-contentful-paint',
      init: {},
      handle: (entries) => {
        const last = entries[entries.length - 1]
        if (last !== undefined) ring.updateVitals({ lcpMs: last.startTime })
      },
    })

    let cls = 0
    observe({
      type: 'layout-shift',
      init: {},
      handle: (entries) => {
        for (const entry of entries as ReadonlyArray<LayoutShiftEntry>) {
          if (!entry.hadRecentInput) cls += entry.value
        }
        ring.updateVitals({ cls })
      },
    })

    const onPageHide = () => {
      const { vitals } = ring.getSnapshot()
      const now = performance.now()
      emit({
        name: 'wf.page.vitals',
        startMs: 0,
        endMs: now,
        attributes: {
          'span.label': 'vitals',
          'wf.vitals.cls': vitals.cls,
          'wf.vitals.long_frames': vitals.longFrames,
          ...(vitals.inpMs === undefined ? {} : { 'wf.vitals.inp_ms': vitals.inpMs }),
          ...(vitals.lcpMs === undefined ? {} : { 'wf.vitals.lcp_ms': vitals.lcpMs }),
        },
      })
      flushRequested = true
      Deferred.doneUnsafe(ready, Effect.void)
      flush?.()
    }
    window.addEventListener('pagehide', onPageHide)
    const onVisibilityChange = () => {
      if (document.visibilityState === 'visible') Deferred.doneUnsafe(ready, Effect.void)
      if (document.visibilityState === 'hidden') onPageHide()
    }
    document.addEventListener('visibilitychange', onVisibilityChange)
    const onStartupError = () => { Deferred.doneUnsafe(ready, Effect.void) }
    window.addEventListener('error', onStartupError)
    window.addEventListener('unhandledrejection', onStartupError)

    uninstall = () => {
      uninstall = undefined
      for (const timer of timers.values()) clearTimeout(timer)
      ux?.dispose()
      disposed = true
      shutdownRequested = true
      Deferred.doneUnsafe(ready, Effect.void)
      closeExporter?.()
      timers.clear()
      for (const cancel of paints) cancel()
      paints.clear()
      pending.length = 0
      for (const type of interactionEvents)
        window.removeEventListener(type, onInteraction, { capture: true })
      window.removeEventListener('pagehide', onPageHide)
      document.removeEventListener('visibilitychange', onVisibilityChange)
      window.removeEventListener('error', onStartupError)
      window.removeEventListener('unhandledrejection', onStartupError)
      for (const observer of observers) observer.disconnect()
    }
    return uninstall
  }

  return { ring, layer, onCommit, install, get ux() { return getUx() } }
}
