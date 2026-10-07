import { createServer } from 'node:http'
import type { IncomingMessage, Server, ServerResponse } from 'node:http'
import { resolve } from 'node:path'
import type { Duplex } from 'node:stream'
import { Cause, Context, Effect, Exit, Fiber, Layer, Schema, Scope, Tracer } from 'effect'
import { completedResponse, reject } from './boundary.mts'
import { clientRoute, Gateway } from './gateway.mts'
import { RuntimeIdentity, StaticAssets } from './static.mts'
import { requestSpanOptions } from './tracing.mts'

/** The host supplies authentication/CSRF policy; no deployment identity is assumed. */
export interface MiddlewareOptions {
  readonly server: Server
  readonly admit: (request: IncomingMessage) => boolean | Promise<boolean>
  readonly gateway: { readonly socketPath: string; readonly host: string; readonly authorization: string; readonly timeoutMs: number }
  readonly tracer: Tracer.Tracer
}
export interface ServerOptions extends Omit<MiddlewareOptions, 'server'> {
  readonly dist: string
  readonly identity: RuntimeIdentity
  readonly port: number
  readonly host: string
}
export interface FractalWebMiddleware {
  middleware(req: IncomingMessage, res: ServerResponse, next: () => void | Promise<void>): Promise<void>
  close(): Promise<void>
}
export interface FractalWebServer {
  readonly server: Server
  listen(): Server
  close(): Promise<void>
}

const makeBoundary = (options: MiddlewareOptions, assets?: { root: string; identity: RuntimeIdentity }) => {
  if (!options.gateway.socketPath || !options.gateway.host || !options.gateway.authorization ||
      !Number.isSafeInteger(options.gateway.timeoutMs) || options.gateway.timeoutMs <= 0)
    throw new TypeError('Explicit paired gateway socket, host, authorization and positive timeout are required')
  const scope = Scope.makeUnsafe()
  try {
    const gateway = Context.get(Effect.runSync(Layer.buildWithScope(Gateway.layer(options.gateway), scope)), Gateway)
    const staticAssets = assets === undefined ? undefined : Context.get(
      Effect.runSync(Layer.buildWithScope(StaticAssets.layer(assets.root, assets.identity), scope)), StaticAssets,
    )
    let closed = false
    let closing: Promise<void> | undefined
    const run = <TValue,>(operation: Effect.Effect<TValue>, downstream: ServerResponse | Duplex): Promise<void> => {
      if (closed) { downstream.destroy(); return Promise.resolve() }
      let fiber: Fiber.Fiber<TValue> | undefined
      const cancel = () => {
        if (!('writableFinished' in downstream) || !downstream.writableFinished) fiber?.interruptUnsafe()
      }
      downstream.once('close', cancel)
      downstream.once('error', cancel)
      fiber = Effect.runSync(Effect.forkIn(operation.pipe(
        Effect.onInterrupt(() => Effect.sync(() => downstream.destroy())),
        Effect.provideService(Tracer.Tracer, options.tracer),
      ), scope, { startImmediately: true }))
      if (downstream.destroyed && (!('writableFinished' in downstream) || !downstream.writableFinished)) fiber.interruptUnsafe()
      return Effect.runPromiseExit(Fiber.join(fiber)).then((exit) => {
        downstream.off('close', cancel)
        downstream.off('error', cancel)
        if (Exit.isFailure(exit) && !Cause.hasInterruptsOnly(exit.cause)) {
          downstream.destroy()
          throw Cause.squash(exit.cause)
        }
      })
    }
    const admitted = async (req: IncomingMessage): Promise<boolean> => {
      try { return !closed && await options.admit(req) }
      catch { return false }
    }
    const handle = async (req: IncomingMessage, res: ServerResponse, next: Effect.Effect<void>): Promise<void> => {
      // Admission refusals and auth-hook failures do not enter tracing.
      if (!await admitted(req)) { reject(res, 403, 'Forbidden\n'); return }
      const dispatch = Effect.gen(function* () {
        const url = req.url ?? ''
        if (clientRoute(url)) {
          const parent = yield* Effect.currentSpan.pipe(Effect.orDie)
          return yield* gateway.http(req, res, parent)
        }
        // No privileged local API or application-specific private routes.
        if (/^\/(?:v1|wf|otlp)(?:\/|\?|$)/.test(url)) { reject(res, 404, 'Not found\n'); return }
        yield* next
      }).pipe(
        Effect.catchDefect(() => Effect.sync(() => {
          if (res.destroyed) return
          if (res.headersSent) res.destroy()
          else reject(res, 500, 'Server failed\n')
        })),
        Effect.andThen(completedResponse(res)),
        Effect.ensuring(Effect.suspend(() => res.writableFinished
          ? Effect.annotateCurrentSpan('http.response.status_code', res.statusCode) : Effect.void)),
        Effect.withSpan('fractal.server.request', requestSpanOptions(req)),
      )
      await run(dispatch, res)
    }
    const upgrade = (req: IncomingMessage, socket: Duplex, head: Buffer): void => {
      // Node relinquishes HTTP error handling as soon as it emits upgrade.
      // Cover pending admission and rejected shutdown, not only admitted tunnels.
      const onError = () => socket.destroy()
      socket.on('error', onError)
      socket.once('close', () => socket.off('error', onError))
      void (async () => {
        const allowed = await admitted(req)
        if (socket.destroyed) return
        if (!allowed || !clientRoute(req.url ?? '') || req.method !== 'GET' || req.headers.upgrade?.toLowerCase() !== 'websocket') {
          socket.end('HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 0\r\n\r\n')
          return
        }
        await run(Effect.gen(function* () {
          const parent = yield* Effect.currentSpan.pipe(Effect.orDie)
          const status = yield* gateway.upgrade(req, socket, head, parent)
          yield* Effect.annotateCurrentSpan('http.response.status_code', status)
        }).pipe(Effect.withSpan('fractal.server.request', requestSpanOptions(req))), socket)
      })().catch(() => socket.destroy())
    }
    options.server.on('upgrade', upgrade)
    return {
      middleware: (req: IncomingMessage, res: ServerResponse, next: () => void | Promise<void>) =>
        handle(req, res, Effect.promise(async () => { await next() })),
      serve: (req: IncomingMessage, res: ServerResponse): Promise<void> => handle(req, res,
        staticAssets === undefined ? Effect.sync(() => reject(res, 404, 'Not found\n')) : staticAssets.handle(req, res)),
      close: (): Promise<void> => {
        if (closing !== undefined) return closing
        closed = true
        options.server.off('upgrade', upgrade)
        closing = Effect.runPromise(Scope.close(scope, Exit.void))
        return closing
      },
    }
  } catch (cause) {
    Effect.runFork(Scope.close(scope, Exit.fail(cause)))
    throw cause
  }
}

export const createFractalWebMiddleware = (options: MiddlewareOptions): FractalWebMiddleware => makeBoundary(options)
export const createFractalWebServer = ({ dist, identity, port, host, ...options }: ServerOptions): FractalWebServer => {
  if (!host || !Number.isSafeInteger(port) || port < 0 || port > 65535)
    throw new TypeError('Explicit listener host and TCP port are required')
  Schema.decodeUnknownSync(RuntimeIdentity)(identity)
  const server = createServer()
  const boundary = makeBoundary({ server, ...options }, { root: resolve(dist), identity })
  server.on('request', (req, res) => { void boundary.serve(req, res).catch(() => res.destroy()) })
  let closing: Promise<void> | undefined
  return {
    server,
    listen: (): Server => server.listen(port, host),
    close: (): Promise<void> => {
      if (closing !== undefined) return closing
      const listener = new Promise<void>((done) => server.close(() => done()))
      server.closeAllConnections()
      closing = boundary.close().then(() => listener)
      return closing
    },
  }
}
