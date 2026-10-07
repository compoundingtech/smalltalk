import type { IncomingMessage } from 'node:http'
import { Tracer } from 'effect'
const methods = new Set(['GET', 'HEAD', 'POST', 'PUT', 'DELETE', 'PATCH', 'OPTIONS', 'CONNECT', 'TRACE'])
export const spanOptions = (route: string, method: string, kind: Tracer.SpanKind = 'internal') => ({
  kind,
  captureStackTrace: false,
  attributes: { 'span.label': route, 'http.route': route, 'http.request.method': methods.has(method) ? method : '_OTHER' },
})
export const incomingParent = (req: IncomingMessage): Tracer.ExternalSpan | undefined => {
  const header = req.headers.traceparent
  if (typeof header !== 'string') return undefined
  const match = /^00-([0-9a-f]{32})-([0-9a-f]{16})-([0-9a-f]{2})$/.exec(header)
  if (!match || /^0+$/.test(match[1]!) || /^0+$/.test(match[2]!)) return undefined
  return Tracer.externalSpan({
    traceId: match[1]!,
    spanId: match[2]!,
    sampled: (Number.parseInt(match[3]!, 16) & 1) === 1,
  })
}
export const traceparent = (span: Tracer.AnySpan): string =>
  `00-${span.traceId}-${span.spanId}-${span.sampled ? '01' : '00'}`

export const serverRoute = (url: string): string => url === '/v1/client' || url.startsWith('/v1/client/') || url.startsWith('/v1/client?') ? '/v1/client/*' : 'assets'
export const requestSpanOptions = (req: IncomingMessage) => {
  const parent = incomingParent(req)
  return { ...spanOptions(serverRoute(req.url ?? '/'), req.method ?? '_OTHER', 'server'), parent, root: parent === undefined }
}
