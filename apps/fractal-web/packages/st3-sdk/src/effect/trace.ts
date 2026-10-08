/**
 * W3C trace context at the browser transport edge.
 *
 * The generated client injects `traceparent`/`tracestate` itself (HTTP headers, socket factory
 * headers, and a `trace` field on collection subscribes). A browser `WebSocket` cannot send
 * headers, so `traceQuerySocket` moves the context into the upgrade URL's query, which the
 * Fractal gateway validates and strips before forwarding. `wrapTraceFetch` gives other HTTP
 * ports (not built on `St3Client`) the same propagation and validation, for the gateway only.
 */
import type { CollectionSocket, CollectionSocketFactory, TraceContext } from '@smalltalk/st3-client'
import type * as Tracer from 'effect/Tracer'

export type { TraceContext } from '@smalltalk/st3-client'

const TRACEPARENT = /^00-([0-9a-f]{32})-([0-9a-f]{16})-[0-9a-f]{2}$/

/** Lowercase W3C version 00 with non-zero trace and span ids, as the generated client accepts. */
export const isValidTraceparent = (value: string): boolean => {
  const match = TRACEPARENT.exec(value)
  return match !== null && !/^0+$/.test(match[1]!) && !/^0+$/.test(match[2]!)
}

/** The `traceparent` naming `span` as the remote parent. */
export const traceparentOf = (span: Tracer.AnySpan): string =>
  `00-${span.traceId}-${span.spanId}-${span.sampled ? '01' : '00'}`

/** The context naming `span` as the remote parent. */
export const traceContextOf = (span: Tracer.AnySpan): TraceContext => ({ traceparent: traceparentOf(span) })

/** The callback's context when valid; an invalid `traceparent` suppresses `tracestate` too. */
export const validTraceContext = (
  traceContext: (() => TraceContext | undefined) | undefined,
): TraceContext | undefined => {
  const context = traceContext?.()
  if (context === undefined || !isValidTraceparent(context.traceparent)) return undefined
  return context.tracestate === undefined
    ? { traceparent: context.traceparent }
    : { traceparent: context.traceparent, tracestate: context.tracestate }
}

/**
 * A `fetch` that adds the callback's context to each request to `baseUrl`'s origin when it starts;
 * other origins never see it. Headers the caller already set win, so wrapping a fetch used by
 * `St3Client` (which injects its own) is harmless.
 */
export const wrapTraceFetch = ({
  fetchImpl,
  traceContext,
  baseUrl,
}: {
  readonly fetchImpl: typeof fetch
  readonly traceContext: () => TraceContext | undefined
  readonly baseUrl: string
}): typeof fetch => {
  // A relative `baseUrl` (and relative request URLs) resolve against the page, as `fetch` does.
  const base = new URL(baseUrl, globalThis.location?.href)
  return (input, init) => {
    const href = input instanceof Request ? input.url : input instanceof URL ? input.href : input
    if (new URL(href, base).origin !== base.origin) return fetchImpl(input, init)
    const context = validTraceContext(traceContext)
    if (context === undefined) return fetchImpl(input, init)
    const headers = new Headers(init?.headers ?? (input instanceof Request ? input.headers : undefined))
    if (headers.has('traceparent')) return fetchImpl(input, init)
    headers.set('traceparent', context.traceparent)
    if (context.tracestate !== undefined) headers.set('tracestate', context.tracestate)
    return fetchImpl(input, { ...init, headers })
  }
}

/** A browser `WebSocket` behind the generated client's socket surface. */
export const browserSocket: CollectionSocketFactory = (url, protocols) => {
  const socket = new WebSocket(url, protocols)
  const wrapped: CollectionSocket = {
    onopen: null,
    onmessage: null,
    onclose: null,
    onerror: null,
    send: (data) => socket.send(data),
    close: (code, reason) => socket.close(code, reason),
  }
  socket.onopen = () => wrapped.onopen?.()
  socket.onmessage = (event) => wrapped.onmessage?.({ data: event.data })
  socket.onclose = (event) => wrapped.onclose?.({ code: event.code, reason: event.reason })
  socket.onerror = (event) => wrapped.onerror?.(event)
  return wrapped
}

/** Carry the factory headers' trace context as upgrade URL query parameters. */
export const traceQuerySocket =
  (inner: CollectionSocketFactory): CollectionSocketFactory =>
  (url, protocols, headers) => {
    const traceparent = headers['traceparent']
    if (traceparent === undefined || !isValidTraceparent(traceparent)) return inner(url, protocols, headers)
    const traced = new URL(url)
    traced.searchParams.set('traceparent', traceparent)
    const tracestate = headers['tracestate']
    if (tracestate !== undefined) traced.searchParams.set('tracestate', tracestate)
    return inner(traced.toString(), protocols, headers)
  }
