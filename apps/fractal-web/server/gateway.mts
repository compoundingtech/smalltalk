import { request } from 'node:http'
import type { IncomingHttpHeaders, IncomingMessage, OutgoingHttpHeaders, ServerResponse } from 'node:http'
import type { Duplex } from 'node:stream'
import { pipeline } from 'node:stream'
import { constants, createBrotliCompress, createGzip } from 'node:zlib'
import { Context, Effect, Layer } from 'effect'
import type { Tracer } from 'effect'
import { BoundaryError, reject } from './boundary.mts'
import { spanOptions, traceparent } from './tracing.mts'
import { encodingQualities } from './static.mts'

const responseCodings = ['br', 'gzip', 'identity'] as const

const hopHeaders = new Set(['connection', 'keep-alive', 'proxy-authenticate', 'proxy-authorization', 'te', 'trailer', 'transfer-encoding', 'upgrade'])
export const cleanHeaders = (headers: IncomingHttpHeaders, upgrade = false): OutgoingHttpHeaders => {
  const blocked = new Set(hopHeaders)
  for (const token of String(headers.connection ?? '').split(',')) blocked.add(token.trim().toLowerCase())
  const result: OutgoingHttpHeaders = {}
  for (const [name, value] of Object.entries(headers)) {
    if (blocked.has(name) || ['authorization', 'origin', 'host', 'cookie', 'traceparent', 'tracestate', 'baggage', 'forwarded'].includes(name) || name.startsWith('x-forwarded-') || name.startsWith('sec-fetch-')) continue
    result[name] = value
  }
  if (upgrade) { result.connection = 'Upgrade'; result.upgrade = 'websocket' }
  return result
}
export const clientRoute = (url: string): boolean => {
  const path = url.split('?')[0] ?? ''
  if (path !== '/v1/client' && !path.startsWith('/v1/client/')) return false
  try {
    const decoded = decodeURIComponent(path)
    return !decoded.includes('\\') && !decoded.includes('\0') && !decoded.split('/').includes('..')
  } catch { return false }
}
const unavailable = (cause?: unknown) => new BoundaryError({ status: 502, code: 'client-gateway-unavailable', message: 'Client gateway unavailable\n', cause })
const makeGateway = ({ socketPath, host, authorization, timeoutMs }: { socketPath: string; host: string; authorization: string; timeoutMs: number }) => Effect.gen(function* () {
  // The service scope, not the handshake fiber, owns successful tunnels.
  const tunnels = yield* Effect.acquireRelease(Effect.sync(() => new Set<Duplex>()), (sockets) => Effect.sync(() => {
    for (const socket of sockets) socket.destroy()
    sockets.clear()
  }))
  const http = (req: IncomingMessage, res: ServerResponse, parent: Tracer.AnySpan) => Effect.gen(function* () {
    const span = yield* Effect.currentSpan.pipe(Effect.orDie)
    const status = yield* Effect.callback<number, BoundaryError>((resume) => {
      let reply: IncomingMessage | undefined
      let settled = false
      const cleanup = () => {
        req.off('aborted', aborted); req.unpipe(upstream)
        reply?.off('error', error); reply?.off('aborted', aborted); reply?.off('close', closed)
      }
      const fail = (cause?: unknown) => {
        if (settled) return
        settled = true
        upstream.destroy(); reply?.destroy(); cleanup()
        resume(Effect.fail(unavailable(cause)))
      }
      const error = (cause: Error) => fail(cause)
      const aborted = () => fail()
      const closed = () => { if (!reply?.complete) fail() }
      const end = () => {
        if (settled) return
        settled = true; cleanup(); resume(Effect.succeed(reply?.statusCode ?? 502))
      }
      const upstream = request({ socketPath, path: req.url, method: req.method, headers: {
        ...cleanHeaders(req.headers), host, authorization, traceparent: traceparent(span),
      }, agent: false }, (incoming) => {
        reply = incoming
        const headers = cleanHeaders(incoming.headers)
        const json = req.method === 'GET' && incoming.statusCode === 200
          && /^application\/json(?:;|$)/i.test(String(headers['content-type'] ?? ''))
          && headers['content-encoding'] === undefined && req.headers.range === undefined
          && !/\bno-transform\b/i.test(String(headers['cache-control'] ?? ''))
        let coding: typeof responseCodings[number] = 'identity'
        if (json) {
          const qualities = encodingQualities(req.headers['accept-encoding'])
          coding = responseCodings.reduce((best, candidate) => qualities[candidate] > qualities[best] ? candidate : best)
          headers.vary = headers.vary === undefined ? 'Accept-Encoding' : `${headers.vary}, Accept-Encoding`
          if (qualities[coding] === 0) {
            incoming.destroy()
            res.setHeader('vary', headers.vary)
            reject(res, 406, 'Not acceptable\n')
            settled = true; cleanup(); resume(Effect.succeed(406))
            return
          }
          if (coding !== 'identity') {
            delete headers['content-length']
            headers['content-encoding'] = coding
            if (typeof headers.etag === 'string' && !headers.etag.startsWith('W/')) headers.etag = `W/${headers.etag}`
          }
        }
        res.writeHead(incoming.statusCode ?? 502, headers)
        incoming.once('error', error); incoming.once('aborted', aborted); incoming.once('close', closed)
        const complete = (cause: NodeJS.ErrnoException | null) => cause === null ? end() : fail(cause)
        if (coding === 'br') pipeline(incoming, createBrotliCompress({
          params: { [constants.BROTLI_PARAM_QUALITY]: 4 }, flush: constants.BROTLI_OPERATION_FLUSH,
        }), res, complete)
        else if (coding === 'gzip') pipeline(incoming, createGzip({ flush: constants.Z_SYNC_FLUSH }), res, complete)
        else pipeline(incoming, res, complete)
      })
      upstream.on('error', error)
      upstream.once('close', () => { upstream.off('error', error); if (!settled && !reply?.complete) fail() })
      req.once('aborted', aborted)
      req.pipe(upstream)
      return Effect.sync(() => { settled = true; upstream.destroy(); reply?.destroy(); cleanup() })
    }).pipe(Effect.timeoutOrElse({ duration: timeoutMs, orElse: () => Effect.fail(unavailable()) }))
    yield* Effect.annotateCurrentSpan('http.response.status_code', status)
  }).pipe(Effect.withSpan('fractal.gateway.request', { ...spanOptions('/v1/client/*', req.method ?? '_OTHER', 'client'), parent }),
    Effect.catchTag('BoundaryError', () => Effect.sync(() => {
      if (res.destroyed) return
      if (res.headersSent) res.destroy()
      else reject(res, 502, 'Client gateway unavailable\n')
    })))

  const upgrade = (req: IncomingMessage, socket: Duplex, head: Buffer, parent: Tracer.AnySpan) => Effect.gen(function* () {
    const span = yield* Effect.currentSpan.pipe(Effect.orDie)
    const status = yield* Effect.callback<number, BoundaryError>((resume) => {
      let settled = false
      const fail = (cause?: unknown) => {
        if (settled) return
        settled = true; upstream.destroy(); cleanup(); resume(Effect.fail(unavailable(cause)))
      }
      const error = (cause: Error) => fail(cause)
      const clientClosed = () => fail()
      const cleanup = () => { socket.off('error', error); socket.off('close', clientClosed) }
      const upstream = request({ socketPath, path: req.url, method: req.method, headers: {
        ...cleanHeaders(req.headers, true), host, authorization, traceparent: traceparent(span),
      }, agent: false })
      socket.once('error', error); socket.once('close', clientClosed)
      upstream.on('error', error)
      upstream.once('close', () => { upstream.off('error', error); if (!settled) fail() })
      upstream.once('response', (reply) => {
        if (settled) { reply.destroy(); return }
        settled = true; cleanup()
        // No rejection body reaches the browser. Release it before completing the handshake.
        reply.destroy(); upstream.destroy()
        socket.end(`HTTP/1.1 ${reply.statusCode ?? 502} Gateway rejected upgrade\r\nConnection: close\r\nContent-Length: 0\r\n\r\n`)
        resume(Effect.succeed(reply.statusCode ?? 502))
      })
      upstream.once('upgrade', (reply, peer, upstreamHead) => {
        if (settled) { peer.destroy(); return }
        settled = true; cleanup()
        tunnels.add(socket); tunnels.add(peer)
        socket.once('close', () => { tunnels.delete(socket); peer.destroy() })
        peer.once('close', () => { tunnels.delete(peer); socket.destroy() })
        socket.on('error', () => peer.destroy()); peer.on('error', () => socket.destroy())
        const headers = cleanHeaders(reply.headers, true)
        socket.write(`HTTP/1.1 101 Switching Protocols\r\n${Object.entries(headers).map(([name, value]) => `${name}: ${value}\r\n`).join('')}\r\n`)
        if (upstreamHead.length !== 0) socket.write(upstreamHead)
        if (head.length !== 0) peer.write(head)
        peer.pipe(socket); socket.pipe(peer)
        resume(Effect.succeed(reply.statusCode ?? 101))
      })
      upstream.end()
      return Effect.sync(() => { settled = true; upstream.destroy(); cleanup() })
    }).pipe(Effect.timeoutOrElse({ duration: timeoutMs, orElse: () => Effect.fail(unavailable()) }))
    yield* Effect.annotateCurrentSpan('http.response.status_code', status)
    return status
  }).pipe(Effect.withSpan('fractal.gateway.request', { ...spanOptions('/v1/client/*', 'GET', 'client'), parent }),
    Effect.catchTag('BoundaryError', () => Effect.sync(() => {
      if (!socket.destroyed) socket.end('HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\nContent-Length: 0\r\n\r\n')
      return 502
    })))
  return { http, upgrade }
})
export interface GatewayOptions { readonly socketPath: string; readonly host: string; readonly authorization: string; readonly timeoutMs: number }
export interface GatewayService {
  http(req: IncomingMessage, res: ServerResponse, parent: Tracer.AnySpan): Effect.Effect<void>
  upgrade(req: IncomingMessage, socket: Duplex, head: Buffer, parent: Tracer.AnySpan): Effect.Effect<number>
}
export class Gateway extends Context.Service<Gateway, GatewayService>()('fractal-web/Gateway') {
  static layer(options: GatewayOptions) { return Layer.effect(this, makeGateway(options)) }
}
