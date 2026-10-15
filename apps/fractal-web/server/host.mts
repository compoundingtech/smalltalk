import type { IncomingMessage } from 'node:http'
import { readFileSync, statSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { Tracer } from 'effect'
import { createFractalWebServer, type FractalWebServer } from './core.mts'

/**
 * A runnable host for one person's web app on a tailnet: a loopback listener that `tailscale serve`
 * publishes over HTTPS. The paired bearer is injected for every admitted request, so admission is
 * the whole authorization: the tailnet login `tailscale serve` attaches must be one of the allowed
 * logins, and any browser Origin must be exactly the published origin. Never publish it with Funnel,
 * which carries no tailnet identity.
 */
export interface TailnetAdmissionOptions {
  /** The exact published origin, such as `https://atlas.example.ts.net:8443`. */
  readonly origin: string
  /** Tailnet login names allowed to act as the paired person. */
  readonly logins: readonly string[]
  /** The loopback listener's own `host:port`, once bound. */
  readonly listener: () => string | undefined
}

const safeMethods = new Set(['GET', 'HEAD'])

export const tailnetAdmission = ({ origin, logins, listener }: TailnetAdmissionOptions) => {
  const published = new URL(origin)
  if (published.origin !== origin || published.protocol !== 'https:') throw new TypeError('The published origin must be an exact https origin')
  if (logins.length === 0) throw new TypeError('At least one tailnet login is required')
  return (request: IncomingMessage): boolean => {
    const login = request.headers['tailscale-user-login']
    if (typeof login !== 'string' || !logins.includes(login)) return false
    const host = request.headers.host
    if (host !== published.host && (host === undefined || host !== listener())) return false
    const requestOrigin = request.headers.origin
    // Browsers send Origin on every upgrade and every unsafe method; only plain reads may omit it,
    // and a browser that says a read came from another site is refused even without an Origin.
    if (requestOrigin === undefined) {
      const site = request.headers['sec-fetch-site']
      return request.headers.upgrade === undefined && safeMethods.has(request.method ?? 'GET') &&
        (site === undefined || site === 'same-origin' || site === 'none')
    }
    return requestOrigin === origin
  }
}

export interface HostConfig {
  readonly dist: string
  readonly host: string
  readonly port: number
  readonly origin: string
  readonly logins: readonly string[]
  readonly socketPath: string
  readonly authorization: string
  readonly deploymentId: string
}

const loopbackHosts = new Set(['127.0.0.1', '::1', 'localhost'])

export interface SecretFiles {
  readonly readText: (path: string) => string
  /** The file's permission bits and owner. */
  readonly owner: (path: string) => { readonly mode: number; readonly uid: number }
}
const localFiles: SecretFiles = { readText: (path) => readFileSync(path, 'utf8'), owner: (path) => statSync(path) }

/**
 * Reads the host's environment. The bearer comes from a file so it never sits in the process
 * environment, and that file must belong to this user and be unreadable by anyone else.
 */
export const readHostConfig = (env: NodeJS.ProcessEnv, files: SecretFiles = localFiles, uid: number | undefined = process.getuid?.()): HostConfig => {
  const required = (name: string): string => {
    const value = env[name]?.trim()
    if (!value) throw new TypeError(`${name} is required`)
    return value
  }
  const host = env.WF_HOST?.trim() || '127.0.0.1'
  if (!loopbackHosts.has(host)) throw new TypeError('WF_HOST must be a loopback address; publish it with tailscale serve')
  const port = Number(required('WF_PORT'))
  if (!Number.isSafeInteger(port) || port <= 0 || port > 65535) throw new TypeError('WF_PORT must be a TCP port')
  const origin = required('WF_PUBLIC_ORIGIN')
  const logins = required('WF_TAILNET_LOGINS').split(',').map((login) => login.trim()).filter((login) => login !== '')
  if (logins.length === 0) throw new TypeError('WF_TAILNET_LOGINS must name at least one tailnet login')
  const bearerFile = required('WF_ST_AUTHORIZATION_FILE')
  const { mode, uid: fileOwner } = files.owner(bearerFile)
  if ((mode & 0o077) !== 0 || (uid !== undefined && fileOwner !== uid))
    throw new TypeError('WF_ST_AUTHORIZATION_FILE must be owned by this user and readable by no one else (chmod 600)')
  const token = files.readText(bearerFile).trim()
  if (token === '' || /\s/.test(token.replace(/^Bearer /, ''))) throw new TypeError('WF_ST_AUTHORIZATION_FILE must hold one bearer token')
  return {
    dist: resolve(env.WF_DIST?.trim() || 'dist/web'),
    host,
    port,
    origin,
    logins,
    socketPath: required('WF_ST_GATEWAY').replace(/^unix:/, ''),
    authorization: token.startsWith('Bearer ') ? token : `Bearer ${token}`,
    deploymentId: env.WF_DEPLOYMENT_ID?.trim() || new URL(origin).host,
  }
}

export const startHost = (config: HostConfig, readText: (path: string) => string = (path) => readFileSync(path, 'utf8')): FractalWebServer => {
  const buildIdentity: unknown = JSON.parse(readText(join(config.dist, 'build-identity.json')))
  let listening: string | undefined
  const served = createFractalWebServer({
    dist: config.dist,
    host: config.host,
    port: config.port,
    admit: tailnetAdmission({ origin: config.origin, logins: config.logins, listener: () => listening }),
    tracer: Tracer.make({ span: (options) => new Tracer.NativeSpan(options) }),
    gateway: { socketPath: config.socketPath, host: '127.0.0.1', authorization: config.authorization, timeoutMs: 30_000 },
    identity: { deploymentId: config.deploymentId, buildIdentity: buildIdentity as never },
  })
  served.server.once('listening', () => {
    const address = served.server.address()
    if (address !== null && typeof address === 'object') listening = `${address.family === 'IPv6' ? `[${address.address}]` : address.address}:${address.port}`
  })
  return served
}
