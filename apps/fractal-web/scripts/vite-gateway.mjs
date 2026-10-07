import { Tracer } from 'effect'

import { createFractalWebMiddleware } from '../server/core.mts'

/** Enable live data only when explicitly configured; Storybook remains credential-free. */
export function webfractalGateway() {
  return {
    name: 'wf:shared-client-gateway',
    apply: 'serve',
    configResolved(config) {
      if (!process.env.WF_ST_GATEWAY) return
      if (!['127.0.0.1', 'localhost'].includes(config.server.host)) {
        throw new Error('WF_ST_GATEWAY requires a loopback Vite listener')
      }
    },
    configureServer(vite) {
      const socket = process.env.WF_ST_GATEWAY
      if (!socket) return
      if (!vite.httpServer) throw new Error('WF_ST_GATEWAY requires a Vite HTTP listener')
      const authorization = process.env.WF_ST_AUTHORIZATION
      if (authorization === undefined || authorization === '') {
        throw new Error('WF_ST_GATEWAY requires WF_ST_AUTHORIZATION to carry the paired gateway bearer token')
      }
      const loopback = new Set(['127.0.0.1', 'localhost'])
      const boundary = createFractalWebMiddleware({
        server: vite.httpServer,
        // Dev admission: an exact loopback Host header only; no deployment identity is assumed.
        admit: (request) => {
          const match = /^([a-z0-9.-]+)(?::([0-9]+))?$/.exec(request.headers.host ?? '')
          return match !== null && loopback.has(match[1]!)
        },
        gateway: {
          socketPath: socket.replace(/^unix:/, ''),
          host: process.env.WF_ST_GATEWAY_HOST ?? '127.0.0.1',
          authorization,
          timeoutMs: 30_000,
        },
        tracer: Tracer.make({ span: (options) => new Tracer.NativeSpan(options) }),
      })
      vite.middlewares.use(boundary.middleware)
      vite.httpServer.once('close', boundary.close)
    },
  }
}
