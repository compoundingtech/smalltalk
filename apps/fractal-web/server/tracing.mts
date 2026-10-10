import type { IncomingMessage } from 'node:http'
import { Context, Schema, Tracer } from 'effect'
const methods = new Set(['GET', 'HEAD', 'POST', 'PUT', 'DELETE', 'PATCH', 'OPTIONS', 'CONNECT', 'TRACE'])
const TraceParent = Schema.String.check(Schema.isMaxLength(55)).check(
  Schema.isPattern(/^00-(?!0{32}-)[0-9a-f]{32}-(?!0{16}-)[0-9a-f]{16}-[0-9a-f]{2}$/),
).annotate({ identifier: 'FractalWeb.TraceParent' })
const stateMember = /^([a-z][a-z0-9_*/-]{0,255}|[a-z0-9][a-z0-9_*/-]{0,240}@[a-z][a-z0-9_*/-]{0,13})=([\x20-\x2b\x2d-\x3c\x3e-\x7e]{0,255}[\x21-\x2b\x2d-\x3c\x3e-\x7e])(?![\s\S])/
const TraceState = Schema.String.check(Schema.makeFilter((state) => {
  if (state.length > 512) return false
  const members = state.split(',')
  if (members.length > 32) return false
  const keys = new Set<string>()
  for (const member of members) {
    const match = stateMember.exec(member.replace(/^[ \t]+|[ \t]+$/g, ''))
    if (match === null || keys.has(match[1]!)) return false
    keys.add(match[1]!)
  }
  return true
}, { expected: 'a bounded W3C tracestate with unique valid members' })).annotate({ identifier: 'FractalWeb.TraceState' })
// Propagation-only annotations: never export vendor state as span attributes.
const PropagatedTraceState = Context.Reference<string | undefined>('fractal-web/PropagatedTraceState', {
  defaultValue: () => undefined,
})
type RequestTraceInput = Pick<IncomingMessage, 'headers' | 'url' | 'method'>
export const spanOptions = (route: string, method: string, kind: Tracer.SpanKind = 'internal') => ({
  kind,
  captureStackTrace: false,
  attributes: { 'span.label': route, 'http.route': route, 'http.request.method': methods.has(method) ? method : '_OTHER' },
})
/** Query context is accepted only at the admitted WebSocket upgrade boundary. Headers take precedence. */
export const incomingParent = (req: RequestTraceInput, upgrade = false): Tracer.ExternalSpan | undefined => {
  const url = req.url ?? ''
  const queryStart = url.indexOf('?')
  const query = upgrade && req.headers.traceparent === undefined && queryStart !== -1
    ? new URLSearchParams(url.slice(queryStart + 1)) : undefined
  const parents = query?.getAll('traceparent')
  const header = query === undefined ? req.headers.traceparent : parents?.length === 1 ? parents[0] : undefined
  const decoded = Schema.decodeUnknownExit(TraceParent)(header)
  if (decoded._tag === 'Failure') return undefined
  const states = query?.getAll('tracestate')
  const state = Schema.decodeUnknownExit(TraceState)(
    query === undefined ? req.headers.tracestate : states?.length === 1 ? states[0] : undefined,
  )
  return Tracer.externalSpan({
    traceId: decoded.value.slice(3, 35),
    spanId: decoded.value.slice(36, 52),
    sampled: (Number.parseInt(decoded.value.slice(53), 16) & 1) === 1,
    annotations: state._tag === 'Success' ? Context.make(PropagatedTraceState, state.value) : undefined,
  })
}
export const traceparent = (span: Tracer.AnySpan): string =>
  `00-${span.traceId}-${span.spanId}-${span.sampled ? '01' : '00'}`
/** Forward the gateway child IDs, never the browser's unvalidated input. */
export const traceHeaders = (span: Tracer.AnySpan): { traceparent: string; tracestate?: string } => {
  const state = Context.get(span.annotations, PropagatedTraceState)
  return { traceparent: traceparent(span), ...(state === undefined ? {} : { tracestate: state }) }
}

export const serverRoute = (url: string): string => url === '/v1/client' || url.startsWith('/v1/client/') || url.startsWith('/v1/client?') ? '/v1/client/*' : 'assets'
export const requestSpanOptions = (req: RequestTraceInput, upgrade = false) => {
  const parent = incomingParent(req, upgrade)
  return { ...spanOptions(serverRoute(req.url ?? '/'), req.method ?? '_OTHER', 'server'), parent,
    annotations: parent?.annotations, root: parent === undefined }
}
