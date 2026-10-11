import { Tracer } from 'effect'

import { createFractalWebMiddleware } from '../server/core.mts'
import { devAdmission } from '../server/devAdmission.mts'

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
      const server = vite.httpServer
      const boundary = createFractalWebMiddleware({
        server,
        // Dev admission: loopback Host plus the exact Vite Origin; no deployment identity is assumed.
        admit: devAdmission(() => {
          const address = server.address()
          return typeof address === 'object' && address !== null ? address.port : undefined
        }),
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
