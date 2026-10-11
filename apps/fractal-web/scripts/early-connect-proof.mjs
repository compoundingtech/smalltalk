#!/usr/bin/env node
/**
 * Browser proof: the early-connect script in `src/web/index.html` sends the roster subscribe
 * before the main module evaluates.
 *
 * Serves the real index.html and Vite-transformed main entry from a finite loopback server.
 * The proof module is withheld until the server receives the early subscribe, then imports and
 * fully evaluates the actual main module before reporting browser-side clocks. The real SDK
 * adopts the socket and renders the real shell. Exits non-zero on violated expectations;
 * always closes the browser session, Vite transforms, server and scratch artifacts.
 *
 *   node apps/fractal-web/scripts/early-connect-proof.mjs
 */
import { spawn } from 'node:child_process'
import { createHash, randomBytes } from 'node:crypto'
import { EventEmitter } from 'node:events'
import { readFileSync } from 'node:fs'
import { mkdir, mkdtemp, rm } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { createServer as createViteServer, loadConfigFromFile } from 'vite'

const DEADLINE_MS = 30_000
const index = readFileSync(fileURLToPath(new URL('../src/web/index.html', import.meta.url)), 'utf8')
if (!index.includes('<script type="module" src="/main.tsx"></script>')) throw new Error('index.html no longer loads /main.tsx')
const page = index.replace('src="/main.tsx"', 'src="/proof-main.js"').replace('<script type="module"', '<script>globalThis.__wfEarlyProofHandle = globalThis.__wfEarlyCollections</script><script type="module"')

/** Its import evaluates the actual app entry and all dependencies, not a stand-in main bundle. */
const proofModule = `
const reportError = (message, stack) => {
  void fetch('/browser-error', { method: 'POST', keepalive: true, body: JSON.stringify({ message, stack }) })
}
window.addEventListener('error', (event) => reportError(event.message, event.error?.stack))
window.addEventListener('unhandledrejection', (event) => reportError(String(event.reason), event.reason?.stack))
const originalConsoleError = console.error.bind(console)
console.error = (...args) => {
  originalConsoleError(...args)
  reportError(args.map(String).join(' '))
}
const importStartedAt = performance.now()
const handle = globalThis.__wfEarlyProofHandle
const framesAtStart = handle?.frames.length
await import('/main.tsx')
const moduleStart = performance.now()
while (!document.querySelector('[data-testid="live-agent-workspace"]'))
  await new Promise((resolve) => setTimeout(resolve, 10))
const result = { importStartedAt, moduleStart, subscribeSentAt: handle?.subscribeSentAt,
  framesAtStart, framesAtTake: handle?.frames.length,
  earlyTaken: globalThis.__wfEarlyCollections === undefined, actualShellRendered: true }
await fetch('/done', { method: 'POST', body: JSON.stringify(result) })
`

const events = []
const connections = []
const commands = []
const upgraded = new Set()
const observed = new EventEmitter()
const lifecycleOrder = []
const reports = []
const browserErrors = []
const nextEvent = (name) => new Promise((resolve) => observed.once(name, resolve))
let releaseModule
const subscribed = new Promise((resolve) => { releaseModule = resolve })
let finish
const done = new Promise((resolve) => { finish = resolve })

const app = fileURLToPath(new URL('..', import.meta.url))
const loaded = await loadConfigFromFile({ command: 'serve', mode: 'development' }, `${app}/vite.config.ts`, app)
const proofMiddleware = (req, res, next) => {
  const url = new URL(req.url ?? '/', 'http://proof')
  if (url.pathname === '/') {
    res.writeHead(200, { 'content-type': 'text/html', 'cache-control': 'no-store' })
    void vite.transformIndexHtml('/', page).then((html) => res.end(html))
  } else if (url.pathname === '/proof-main.js') {
    events.push('module-requested')
    const timer = setTimeout(() => { res.writeHead(503); res.end() }, DEADLINE_MS)
    void subscribed.then(() => {
      clearTimeout(timer)
      events.push('module-served')
      res.writeHead(200, { 'content-type': 'text/javascript', 'cache-control': 'no-store' })
      res.end(proofModule)
    })
  } else if (url.pathname === '/done' && req.method === 'POST') {
    let body = ''
    req.on('data', (chunk) => { body += chunk })
    req.on('end', () => {
      const report = JSON.parse(body)
      reports.push(report)
      res.writeHead(204)
      res.end()
      finish(report)
      observed.emit(`report:${reports.length}`, report)
    })
  } else if (url.pathname === '/browser-error' && req.method === 'POST') {
    let body = ''
    req.on('data', (chunk) => { body += chunk })
    req.on('end', () => { browserErrors.push(JSON.parse(body)); res.writeHead(204); res.end() })
  } else if (url.pathname === '/v1/client/capabilities') {
    res.writeHead(200, { 'content-type': 'application/json' })
    res.end(JSON.stringify({ api_version: 'st3.client.v0', snapshot: { id: 'snapshot/proof', created_at: '2026-10-08T00:00:00Z', host_id: 'host/proof', projection_version: 'client-projection.v0', store_index: 1 }, value: { capabilities: [] } }))
  } else if (url.pathname === '/otlp/v1/traces') {
    req.resume()
    res.writeHead(204)
    res.end()
  } else if (url.pathname === '/favicon.ico') {
    res.writeHead(204)
    res.end()
  } else {
    next()
  }
}
const vite = await createViteServer({
  ...loaded.config,
  configFile: false,
  root: `${app}/src/web`,
  plugins: [
    { name: 'early-connect-proof-network', configureServer: (server) => { server.middlewares.use(proofMiddleware) } },
    // The fixture owns the boundary; never connect to an inherited live gateway.
    ...loaded.config.plugins.flat().filter((plugin) => plugin?.name !== 'wf:shared-client-gateway'),
  ],
  server: { ...loaded.config.server, host: '127.0.0.1', port: 0, strictPort: false, hmr: false, preTransformRequests: false },
})
const server = vite.httpServer

/** Text frames from a masked client stream; returns the unconsumed tail. */
const readFrames = (buffer, onText, onClose) => {
  let offset = 0
  for (;;) {
    if (buffer.length - offset < 2) break
    const opcode = buffer[offset] & 0x0f
    let length = buffer[offset + 1] & 0x7f
    let header = 2
    if (length === 126) { if (buffer.length - offset < 4) break; length = buffer.readUInt16BE(offset + 2); header = 4 }
    else if (length === 127) { if (buffer.length - offset < 10) break; length = Number(buffer.readBigUInt64BE(offset + 2)); header = 10 }
    if (buffer.length - offset < header + 4 + length) break
    const mask = buffer.subarray(offset + header, offset + header + 4)
    const payload = Buffer.from(buffer.subarray(offset + header + 4, offset + header + 4 + length))
    for (let index = 0; index < payload.length; index += 1) payload[index] ^= mask[index % 4]
    if (opcode === 1) onText(payload.toString('utf8'))
    if (opcode === 8) onClose(payload.length >= 2 ? payload.readUInt16BE(0) : 1005)
    offset += header + 4 + length
  }
  return buffer.subarray(offset)
}
const textFrame = (text) => {
  const payload = Buffer.from(text)
  const header = payload.length < 126
    ? Buffer.from([0x81, payload.length])
    : Buffer.from([0x81, 126, payload.length >> 8, payload.length & 0xff])
  return Buffer.concat([header, payload])
}

server.on('upgrade', (req, socket) => {
  const url = new URL(req.url ?? '/', 'http://proof')
  if (url.pathname !== '/v1/client/collections/stream') return // Vite owns its own HMR upgrade.
  socket.on('error', () => {})
  upgraded.add(socket)
  socket.on('close', () => upgraded.delete(socket))
  connections.push({ traceparent: url.searchParams.get('traceparent'), protocol: req.headers['sec-websocket-protocol'] })
  const ordinal = connections.length - 1
  events.push('socket-opened')
  const accept = createHash('sha1').update(`${req.headers['sec-websocket-key']}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`).digest('base64')
  socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\nSec-WebSocket-Protocol: st3.client.collections.v0\r\n\r\n`)
  let pending = Buffer.alloc(0)
  socket.on('data', (chunk) => {
    pending = readFrames(Buffer.concat([pending, chunk]), (text) => {
      const command = JSON.parse(text)
      commands.push({ connection: ordinal, ...command })
      if (command.kind !== 'subscribe') return
      events.push('subscribe-received')
      if (command.collection === 'agents') {
        lifecycleOrder.push(`roster:${ordinal}`)
        observed.emit(`roster:${ordinal}`)
      }
      socket.write(textFrame(JSON.stringify({
        kind: 'snapshot', id: command.id, collection: command.collection, has_more: false, items: [], order: [],
        snapshot: { id: 'snapshot/proof', created_at: '2026-10-08T00:00:00Z', host_id: 'host/proof', projection_version: 'client-projection.v0', store_index: 1 },
      })))
      releaseModule()
    }, (code) => {
      lifecycleOrder.push(`close:${ordinal}:${code}`)
      observed.emit(`close:${ordinal}`, code)
      socket.end(code === 1005 ? Buffer.from([0x88, 0]) : Buffer.from([0x88, 0x02, code >> 8, code & 0xff]))
    })
  })
})
const scratchRoot = fileURLToPath(new URL('../../../tmp', import.meta.url))
await mkdir(scratchRoot, { recursive: true })
const scratch = await mkdtemp(`${scratchRoot}/early-connect-proof-`)

const run = (args) => new Promise((resolve) => {
  const child = spawn('playwright-cli', args, { cwd: scratch, stdio: ['ignore', 'inherit', 'inherit'] })
  child.on('exit', (code) => resolve(code))
  child.on('error', () => resolve(-1))
})

const session = `early-connect-${randomBytes(4).toString('hex')}`
await vite.listen()
const { port } = server.address()
const failures = []
const expect = (condition, message) => { if (!condition) failures.push(message) }
let result
try {
  const opening = run([`-s=${session}`, 'open', `http://127.0.0.1:${port}/`])
  result = await Promise.race([
    (async () => {
      const initial = await done
      await opening
      const cachedClose = nextEvent('close:0')
      await run([`-s=${session}`, 'eval', `() => window.dispatchEvent(new PageTransitionEvent('pagehide', { persisted: true }))`])
      expect(await cachedClose === 1005, 'persisted pagehide did not synchronously send an empty close frame')
      const restoredRoster = nextEvent('roster:1')
      await run([`-s=${session}`, 'eval', `() => window.dispatchEvent(new PageTransitionEvent('pageshow', { persisted: true }))`])
      await restoredRoster
      const navigationClose = nextEvent('close:1')
      const newRoster = nextEvent('roster:2')
      const newReport = nextEvent('report:2')
      await run([`-s=${session}`, 'reload'])
      expect([1005, 1001].includes(await navigationClose), 'navigation pagehide did not send a close frame')
      await newRoster
      await newReport
      return initial
    })(),
    new Promise((resolve) => setTimeout(() => resolve(undefined), DEADLINE_MS).unref()),
  ])
} finally {
  await run([`-s=${session}`, 'close'])
  for (const socket of upgraded) socket.destroy()
  server.closeAllConnections()
  await vite.close()
  await rm(scratch, { recursive: true, force: true })
}

const subscribes = commands.filter((command) => command.kind === 'subscribe' && command.collection === 'agents')
expect(result !== undefined, 'the proof module never reported (subscribe never sent before evaluation?)')
expect(connections.length === 3, `expected initial, restored and reloaded sockets, saw ${connections.length}`)
expect(/^00-[0-9a-f]{32}-[0-9a-f]{16}-01$/.test(connections[0]?.traceparent ?? ''), 'upgrade URL lacks a valid traceparent')
expect(connections.every((connection) => /^00-[0-9a-f]{32}-[0-9a-f]{16}-01$/.test(connection.traceparent ?? '')), 'a resumed upgrade lost W3C trace context')
expect(connections[0]?.protocol === 'st3.client.collections.v0', 'upgrade lacks the collections subprotocol')
expect(subscribes.length === 3, `expected one roster subscribe per initial/restore/reload, saw ${subscribes.length}`)
expect(/^wf-early-[0-9a-f]{16}$/.test(subscribes[0]?.id ?? ''), 'early subscribe id is not the bounded early id')
expect(subscribes[0]?.collection === 'agents' && subscribes[0]?.limit === 100, 'early subscribe is not the fleet roster window')
expect(subscribes[0]?.trace?.traceparent === connections[0]?.traceparent, 'subscribe trace differs from the upgrade trace')
expect(events.indexOf('subscribe-received') !== -1 && events.indexOf('subscribe-received') < events.indexOf('module-served'), `module served before subscribe: ${events.join(',')}`)
expect(result?.subscribeSentAt !== undefined && result.subscribeSentAt < result.moduleStart, 'browser clock: subscribe not sent before module evaluation started')
expect((result?.framesAtTake ?? 0) >= 1, 'the snapshot was not buffered for adoption')
expect(result?.earlyTaken === true, 'the real SDK did not take the early socket')
expect(result?.actualShellRendered === true, 'the real main bundle did not render the shell')
expect(lifecycleOrder.indexOf('close:0:1005') >= 0 && lifecycleOrder.indexOf('close:0:1005') < lifecycleOrder.indexOf('roster:1'), 'bfcache restore subscribed before old close')
expect(lifecycleOrder.findIndex((event) => event.startsWith('close:1:')) >= 0 && lifecycleOrder.findIndex((event) => event.startsWith('close:1:')) < lifecycleOrder.indexOf('roster:2'), 'new document subscribed before old close')
expect(browserErrors.length === 0, 'browser lifecycle raised errors: ' + JSON.stringify(browserErrors))

console.log(JSON.stringify({ events, lifecycleOrder, connections, subscribes, result, browserErrors }, null, 2))
if (failures.length > 0) {
  for (const failure of failures) console.error(`FAIL ${failure}`)
  process.exit(1)
}
console.log('PASS early roster subscribe precedes actual main-module evaluation and SDK adoption')
console.log('PASS synchronous pagehide close frame precedes restore and next-document roster subscribes (native browser API cannot send reserved code 1001)')
console.log('PASS initial render, retained restore and real reload have no console errors or unhandled browser errors')
