import type { IncomingMessage } from 'node:http'

const loopback = ['127.0.0.1', 'localhost']
const hostPattern = /^([a-z0-9.-]+)(?::([0-9]+))?$/

/**
 * Dev-server admission for the authenticated gateway proxy.
 *
 * The Host header must name the loopback listener, and any browser Origin must be exactly the
 * Vite origin on the listening port. WebSocket upgrades must carry that Origin: the proxy
 * injects the paired bearer, so a page on any other origin (including another loopback port)
 * must never reach it. HTTP requests without an Origin (same-origin GETs, non-browser tools on
 * the loopback listener) remain admitted.
 */
export const devAdmission = (port: () => number | undefined) => (request: IncomingMessage): boolean => {
  const host = hostPattern.exec(request.headers.host ?? '')
  if (host === null || !loopback.includes(host[1]!)) return false
  const listening = port()
  if (listening === undefined) return false
  const origin = request.headers.origin
  if (origin === undefined) return request.headers.upgrade === undefined
  return loopback.some((name) => origin === `http://${name}:${listening}`)
}
