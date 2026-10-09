import { afterEach, expect, it } from 'vitest'
import { createHash } from 'node:crypto'
import { createServer } from 'node:http'
import type { IncomingMessage, OutgoingHttpHeaders, Server } from 'node:http'
import { connect } from 'node:net'
import { once } from 'node:events'
import { mkdtemp, mkdir, rm, symlink, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import type { Duplex } from 'node:stream'
import { brotliCompressSync } from 'node:zlib'
import { Option, Tracer } from 'effect'
import { createFractalWebServer } from './core.mts'
import type { FractalWebServer } from './core.mts'
import { cleanHeaders, clientRoute } from './gateway.mts'
import { traceparent } from './tracing.mts'
// Assembled so the source itself never carries a literal bearer credential pair.
const gatewayAuthorization = 'Bearer' + ' fx-auth'
const browserAuthorization = 'Bearer' + ' fx-input'
const browserTraceId = '4bf92f3577b34da6a3ce929d0e0e4736'
const browserSpanId = '00f067aa0ba902b7'
const browserTraceparent = `00-${browserTraceId}-${browserSpanId}-01`

const cleanups: (() => Promise<void>)[] = []
afterEach(async () => { while (cleanups.length) await cleanups.pop()?.() })
const listen = async (server: Server, path?: string): Promise<void> => {
  if (path === undefined) server.listen(0, '127.0.0.1')
  else server.listen(path)
  await once(server, 'listening')
}
const fixture = async (
  admit: (request: IncomingMessage) => boolean | Promise<boolean> = () => true,
  responseBody: string | Buffer = '{"paired":true}',
  responseHeaders: OutgoingHttpHeaders = { 'content-type': 'application/json' },
) => {
  const root = await mkdtemp(join(tmpdir(), 'fractal-server-test-'))
  cleanups.push(() => rm(root, { recursive: true, force: true }))
  const dist = join(root, 'dist')
  await mkdir(dist)
  await writeFile(join(dist, 'index.html'), '<html><head></head><body>fixture</body></html>')
  await writeFile(join(dist, 'main.js'), 'export const fixture = true')
  await writeFile(join(dist, 'main.js.br'), brotliCompressSync('export const fixture = true'))
  await writeFile(join(root, 'outside.txt'), 'not a public asset')
  await symlink(join(root, 'outside.txt'), join(dist, 'outside.txt'))
  const sockets = new Set<Duplex>()
  const received: { path: string; auth?: string; cookie?: string; origin?: string; baggage?: string | string[]; trace?: string | string[]; state?: string | string[]; host?: string; body: string }[] = []
  const gateway = createServer((req, res) => { void (async () => {
    const chunks: Buffer[] = []
    for await (const chunk of req) chunks.push(Buffer.from(chunk))
    received.push({ path: req.url ?? '', auth: req.headers.authorization, cookie: req.headers.cookie, origin: req.headers.origin, baggage: req.headers.baggage, trace: req.headers.traceparent, state: req.headers.tracestate, host: req.headers.host, body: Buffer.concat(chunks).toString('utf8') })
    res.writeHead(200, { 'content-length': Buffer.byteLength(responseBody), ...responseHeaders }); res.end(responseBody)
  })().catch(() => res.destroy()) })
  gateway.on('upgrade', (req, socket, head) => {
    sockets.add(socket)
    socket.on('close', () => sockets.delete(socket))
    socket.on('error', () => undefined)
    received.push({ path: req.url ?? '', auth: req.headers.authorization, cookie: req.headers.cookie, origin: req.headers.origin, baggage: req.headers.baggage, trace: req.headers.traceparent, state: req.headers.tracestate, body: 'upgrade' })
    socket.write('HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n')
    if (head.length) socket.write(head)
    socket.on('data', (chunk) => socket.write(chunk))
  })
  const socketPath = join(root, 'gateway.sock')
  await listen(gateway, socketPath)
  cleanups.push(async () => { for (const socket of sockets) socket.destroy(); gateway.closeAllConnections(); await new Promise<void>((done) => gateway.close(() => done())) })
  const spans: Tracer.Span[] = []
  const tracer = Tracer.make({ span: (options) => { const span = new Tracer.NativeSpan(options); spans.push(span); return span } })
  const app: FractalWebServer = createFractalWebServer({ dist, host: '127.0.0.1', port: 0, admit, tracer,
    gateway: { socketPath, host: 'paired-gateway', authorization: gatewayAuthorization, timeoutMs: 1000 },
    identity: { deploymentId: 'fixture-build', buildIdentity: { baseVersion: '1.0.0', displayVersion: '1.0.0-test', machineVersion: '1.0.0+test', sourceKind: 'local', dirty: false } },
  })
  app.listen()
  await once(app.server, 'listening')
  cleanups.push(() => app.close())
  const address = app.server.address()
  if (address === null || typeof address === 'string') throw new TypeError('Fixture listener is missing')
  return { root, app, gateway, port: address.port, base: `http://127.0.0.1:${address.port}`, received, spans, sockets }
}
const openUpgrade = async (port: number, path: string, headers: Record<string, string> = {}): Promise<string> => {
  const socket = connect(port, '127.0.0.1')
  socket.on('error', () => undefined)
  cleanups.push(async () => { socket.destroy() })
  await once(socket, 'connect')
  const reply = new Promise<string>((resolve, fail) => {
    let value = ''
    const cleanup = () => { socket.off('data', data); socket.off('error', error); socket.off('close', closed) }
    const error = (cause: Error) => { cleanup(); fail(cause) }
    const closed = () => { cleanup(); fail(new TypeError('Upgrade closed before its handshake')) }
    const data = (chunk: Buffer) => {
      value += chunk.toString()
      if (value.includes('\r\n\r\n')) { cleanup(); resolve(value) }
    }
    socket.on('data', data); socket.once('error', error); socket.once('close', closed)
  })
  socket.write(`GET ${path} HTTP/1.1\r\nHost: fixture\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n${Object.entries(headers).map(([name, value]) => `${name}: ${value}\r\n`).join('')}\r\n`)
  return reply
}
const requestSpans = (spans: readonly Tracer.Span[]) => {
  const server = spans.filter((span) => span.name === 'fractal.server.request')
  const gateway = spans.filter((span) => span.name === 'fractal.gateway.request')
  expect(server).toHaveLength(1)
  expect(gateway).toHaveLength(1)
  if (server[0] === undefined || gateway[0] === undefined) throw new TypeError('Expected server and gateway spans')
  expect(Option.getOrUndefined(gateway[0].parent)).toBe(server[0])
  expect(gateway[0].traceId).toBe(server[0].traceId)
  return { server: server[0], gateway: gateway[0] }
}

it('restricts HTML, assets, HEAD and cached responses with the same image policy', async () => {
  const setup = await fixture()
  const response = await fetch(setup.base)
  const html = await response.text()
  const script = html.match(/<script>([\s\S]*?)<\/script>/)?.[1]
  expect(script).toBeDefined()
  const hash = createHash('sha256').update(script!).digest('base64')
  const expected = [
    "default-src 'self'",
    `script-src 'self' 'wasm-unsafe-eval' 'sha256-${hash}'`,
    "style-src 'self' 'sha256-38RhXrc7EdReTKsOm23ZPOCUgniTUUcjky8QOOrQx6o=' 'sha256-gYiS/BvZvRcK27JIXTuwhZ3hs2+VJ1X+2gUlE+farlg='",
    "style-src-attr 'unsafe-inline'",
    "img-src 'self' data: blob:",
    "font-src 'self'",
    `connect-src 'self' ws://127.0.0.1:${setup.port} wss://127.0.0.1:${setup.port}`,
    "object-src 'none'",
    "base-uri 'self'",
    "frame-ancestors 'none'",
    "form-action 'self'",
  ].join('; ')
  expect(response.headers.get('content-security-policy')).toBe(expected)
  for (const path of ['/main.js', '/build-identity.json', '/route-without-extension']) {
    expect((await fetch(setup.base + path)).headers.get('content-security-policy')).toBe(expected)
    expect((await fetch(setup.base + path, { method: 'HEAD' })).headers.get('content-security-policy')).toBe(expected)
  }
  const cached = await fetch(setup.base, { headers: { 'if-none-match': response.headers.get('etag')! } })
  expect(cached.status).toBe(304)
  expect(cached.headers.get('content-security-policy')).toBe(expected)
})

it('proxies only the paired client API, streams bodies and injects trusted credentials/traces', async () => {
  const setup = await fixture()
  const response = await fetch(`${setup.base}/v1/client/actions?test=1`, { method: 'POST', body: '{"fixture":true}', headers: { authorization: browserAuthorization, cookie: 'browser=secret', origin: 'https://example.test' } })
  expect(await response.json()).toEqual({ paired: true })
  expect(setup.received).toEqual([expect.objectContaining({ path: '/v1/client/actions?test=1', auth: gatewayAuthorization, cookie: undefined, body: '{"fixture":true}', host: 'paired-gateway', trace: expect.stringMatching(/^00-[0-9a-f]{32}-[0-9a-f]{16}-/) })])
  for (const path of ['/v1/claims', '/wf/folders', '/otlp/v1/traces']) expect((await fetch(setup.base + path)).status).toBe(404)
  expect(setup.received).toHaveLength(1)
  expect(setup.spans.some((span) => span.name === 'fractal.gateway.request')).toBe(true)
})
it.each([
  ['br, gzip', 'br'],
  ['gzip, br;q=0', 'gzip'],
  ['br;q=0, gzip;q=0', undefined],
  ['br;q=0.1, gzip;q=0.5, identity;q=0', 'gzip'],
] as const)('negotiates streaming roster JSON compression (%s)', async (accept, coding) => {
  const body = JSON.stringify({ items: Array.from({ length: 100 }, (_, index) => ({ id: `agent/${index}`, name: `Agent ${index}`, details: 'reported observation '.repeat(100) })) })
  const setup = await fixture(undefined, body, { 'content-type': 'application/json', etag: '"published"', vary: 'Origin' })
  const response = await fetch(`${setup.base}/v1/client/agents?limit=100`, { headers: { 'accept-encoding': accept } })
  expect(response.headers.get('content-encoding')).toBe(coding ?? null)
  expect(response.headers.get('vary')).toBe('Origin, Accept-Encoding')
  expect(response.headers.get('content-length')).toBe(coding === undefined ? String(Buffer.byteLength(body)) : null)
  expect(response.headers.get('etag')).toBe(coding === undefined ? '"published"' : 'W/"published"')
  expect(await response.text()).toBe(body)
})
it('does not encode JSON when every representation is refused', async () => {
  const setup = await fixture()
  const response = await fetch(`${setup.base}/v1/client/agents`, { headers: { 'accept-encoding': '*;q=0' } })
  expect(response.status).toBe(406)
  expect(response.headers.get('vary')).toBe('Accept-Encoding')
})
it.each([
  { 'content-type': 'text/event-stream' },
  { 'content-type': 'application/json', 'cache-control': 'no-transform' },
])('leaves streaming and no-transform responses untouched (%j)', async headers => {
  const setup = await fixture(undefined, 'data: reported\n\n', headers)
  const response = await fetch(`${setup.base}/v1/client/collections/stream`, { headers: { 'accept-encoding': 'br, gzip' } })
  expect(response.headers.get('content-encoding')).toBeNull()
  expect(await response.text()).toBe('data: reported\n\n')
})
it('never recompresses an encoded upstream JSON response', async () => {
  const body = '{"paired":true}'
  const setup = await fixture(undefined, brotliCompressSync(body), { 'content-type': 'application/json', 'content-encoding': 'br' })
  const response = await fetch(`${setup.base}/v1/client/agents`, { headers: { 'accept-encoding': 'br, gzip' } })
  expect(response.headers.get('content-encoding')).toBe('br')
  expect(await response.text()).toBe(body)
})

it.each(['01', '00'])('continues browser HTTP context into the server and gateway child (%s)', async (flags) => {
  const setup = await fixture()
  const parent = `00-${browserTraceId}-${browserSpanId}-${flags}`
  const response = await fetch(`${setup.base}/v1/client/capabilities`, { headers: {
    traceparent: parent, tracestate: 'fixture=browser', authorization: browserAuthorization,
    cookie: 'browser=secret', origin: 'https://example.test', baggage: 'private=browser',
  } })
  expect(await response.json()).toEqual({ paired: true })
  const spans = requestSpans(setup.spans)
  expect(Option.getOrUndefined(spans.server.parent)).toEqual(expect.objectContaining({
    _tag: 'ExternalSpan', traceId: browserTraceId, spanId: browserSpanId, sampled: flags === '01',
  }))
  expect(spans.gateway.traceId).toBe(browserTraceId)
  expect(spans.gateway.sampled).toBe(flags === '01')
  expect(setup.received).toEqual([expect.objectContaining({
    trace: traceparent(spans.gateway), state: 'fixture=browser', auth: gatewayAuthorization,
    cookie: undefined, origin: undefined, baggage: undefined,
  })])
  expect(setup.received[0]?.trace).not.toBe(parent)
})
it.each([
  `00-${'0'.repeat(32)}-${browserSpanId}-01`,
  `00-${browserTraceId}-${'0'.repeat(16)}-01`,
  `${browserTraceparent},${browserTraceparent}`,
  browserTraceparent.toUpperCase(),
  browserTraceparent.replace(/^00/, 'ff'),
])('discards malformed browser HTTP context and forwards a new trusted child', async (parent) => {
  const setup = await fixture()
  const response = await fetch(`${setup.base}/v1/client/capabilities`, { headers: { traceparent: parent, tracestate: 'fixture=untrusted' } })
  expect(await response.json()).toEqual({ paired: true })
  const spans = requestSpans(setup.spans)
  expect(Option.isNone(spans.server.parent)).toBe(true)
  expect(setup.received[0]?.trace).toBe(traceparent(spans.gateway))
  expect(setup.received[0]?.state).toBeUndefined()
  expect(spans.gateway.traceId).not.toBe(browserTraceId)
})
it('continues valid HTTP context but never forwards invalid vendor state', async () => {
  const setup = await fixture()
  const response = await fetch(`${setup.base}/v1/client/capabilities`, { headers: {
    traceparent: browserTraceparent, tracestate: 'fixture=first,fixture=duplicate',
  } })
  expect(await response.json()).toEqual({ paired: true })
  const spans = requestSpans(setup.spans)
  expect(spans.gateway.traceId).toBe(browserTraceId)
  expect(setup.received[0]?.trace).toBe(traceparent(spans.gateway))
  expect(setup.received[0]?.state).toBeUndefined()
})
it('continues browser WebSocket query context and strips it before the upstream handshake', async () => {
  const setup = await fixture()
  const path = `/v1/client/collections/stream?kind=agent&cursor=a%2fb+%20&traceparent=${browserTraceparent}&tracestate=fixture%3Dbrowser`
  expect(await openUpgrade(setup.port, path, {
    Authorization: browserAuthorization, Cookie: 'browser=secret', Origin: 'https://example.test', Baggage: 'private=browser',
  })).toContain('101 Switching Protocols')
  const spans = requestSpans(setup.spans)
  expect(Option.getOrUndefined(spans.server.parent)).toEqual(expect.objectContaining({
    _tag: 'ExternalSpan', traceId: browserTraceId, spanId: browserSpanId,
  }))
  expect(spans.gateway.traceId).toBe(browserTraceId)
  expect(setup.received).toEqual([expect.objectContaining({
    path: '/v1/client/collections/stream?kind=agent&cursor=a%2fb+%20', trace: traceparent(spans.gateway),
    state: 'fixture=browser', auth: gatewayAuthorization, cookie: undefined, origin: undefined, baggage: undefined,
  })])
  expect(setup.received[0]?.trace).not.toBe(browserTraceparent)
  for (const span of [spans.server, spans.gateway]) {
    expect(span.attributes.get('http.route')).toBe('/v1/client/*')
    expect(span.attributes.get('span.label')).toBe('/v1/client/*')
    expect([...span.attributes.values()].some((value) => typeof value === 'string' && value.includes('traceparent='))).toBe(false)
  }
})
it('continues WebSocket header context rather than a conflicting query context', async () => {
  const setup = await fixture()
  expect(await openUpgrade(setup.port, `/v1/client/collections/stream?traceparent=invalid&tracestate=untrusted`, {
    Traceparent: browserTraceparent, Tracestate: 'fixture=header',
  })).toContain('101 Switching Protocols')
  const spans = requestSpans(setup.spans)
  expect(Option.getOrUndefined(spans.server.parent)?.spanId).toBe(browserSpanId)
  expect(spans.gateway.traceId).toBe(browserTraceId)
  expect(setup.received[0]).toEqual(expect.objectContaining({
    path: '/v1/client/collections/stream', trace: traceparent(spans.gateway), state: 'fixture=header',
  }))
})
it.each([
  'traceparent=invalid',
  `traceparent=00-${'0'.repeat(32)}-${browserSpanId}-01`,
  `traceparent=${browserTraceparent}&traceparent=${browserTraceparent}`,
  `traceparent=${browserTraceparent}%0D%0AX-Private%3Ainjected`,
])('discards invalid WebSocket query context without forwarding it', async (query) => {
  const setup = await fixture()
  expect(await openUpgrade(setup.port, `/v1/client/collections/stream?kind=agent&${query}&tracestate=fixture%3Duntrusted`)).toContain('101 Switching Protocols')
  const spans = requestSpans(setup.spans)
  expect(Option.isNone(spans.server.parent)).toBe(true)
  expect(setup.received[0]).toEqual(expect.objectContaining({
    path: '/v1/client/collections/stream?kind=agent', trace: traceparent(spans.gateway), state: undefined,
  }))
  expect(spans.gateway.traceId).not.toBe(browserTraceId)
})
it('rejects WebSocket query header injection while continuing its valid browser trace', async () => {
  const setup = await fixture()
  expect(await openUpgrade(setup.port, `/v1/client/collections/stream?traceparent=${browserTraceparent}&tracestate=fixture%3Dx%0D%0AX-Private%3Ainjected`)).toContain('101 Switching Protocols')
  const spans = requestSpans(setup.spans)
  expect(spans.gateway.traceId).toBe(browserTraceId)
  expect(setup.received[0]).toEqual(expect.objectContaining({
    path: '/v1/client/collections/stream', trace: traceparent(spans.gateway), state: undefined,
  }))
})
it('fails closed for auth-hook refusal and failures, without tracing or proxying', async () => {
  const setup = await fixture(async (req) => { if (req.url === '/throws') throw new TypeError('auth failure'); return false })
  expect((await fetch(`${setup.base}/v1/client/capabilities`)).status).toBe(403)
  expect((await fetch(`${setup.base}/throws`)).status).toBe(403)
  expect(setup.spans).toHaveLength(0)
  expect(setup.received).toHaveLength(0)
})
it('serves SPA/HEAD/conditional/precompressed assets with safe caching and traversal isolation', async () => {
  const setup = await fixture()
  const spa = await fetch(`${setup.base}/workspace/selection`)
  expect(await spa.text()).toContain('__BUILD_DEPLOYMENT_ID__')
  expect(spa.headers.get('cache-control')).toBe('no-cache')
  const asset = await fetch(`${setup.base}/main.js`, { headers: { 'accept-encoding': 'br' } })
  expect(asset.headers.get('content-encoding')).toBe('br')
  expect(await asset.text()).toBe('export const fixture = true')
  const conditional = await fetch(`${setup.base}/main.js`, { headers: { 'accept-encoding': 'br', 'if-none-match': asset.headers.get('etag')! } })
  expect(conditional.status).toBe(304)
  expect(await conditional.text()).toBe('')
  expect((await fetch(`${setup.base}/main.js`, { method: 'HEAD' })).headers.get('content-length')).toBe(String(Buffer.byteLength('export const fixture = true')))
  expect((await fetch(`${setup.base}/main.js`, { headers: { 'accept-encoding': '*;q=0' } })).status).toBe(406)
  expect((await fetch(`${setup.base}/outside.txt`)).status).toBe(404)
  expect((await fetch(`${setup.base}/%2e%2e%2foutside.txt`)).status).toBe(404)
})
it('preserves HTTP/WS head bytes and releases successful tunnels on close', async () => {
  const setup = await fixture()
  const socket = connect(setup.port, '127.0.0.1')
  socket.on('error', () => undefined)
  cleanups.push(async () => { socket.destroy() })
  await once(socket, 'connect')
  const reply = new Promise<string>((resolve) => { let value = ''; socket.on('data', (chunk) => { value += chunk.toString(); if (value.includes('head-fixture')) resolve(value) }) })
  socket.write('GET /v1/client/collections/stream HTTP/1.1\r\nHost: fixture\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\nhead-fixture')
  expect(await reply).toContain('101 Switching Protocols')
  const closed = once(socket, 'close')
  await setup.app.close()
  await closed
  expect(setup.received[0]?.auth).toBe(gatewayAuthorization)
  expect(setup.spans.filter((span) => span.name === 'fractal.gateway.request')).toHaveLength(1)
})
it('rejects websocket admission before the upstream handshake', async () => {
  const setup = await fixture(() => false)
  const socket = connect(setup.port, '127.0.0.1')
  socket.on('error', () => undefined)
  cleanups.push(async () => { socket.destroy() })
  await once(socket, 'connect')
  const reply = once(socket, 'data')
  socket.write('GET /v1/client/collections/stream HTTP/1.1\r\nHost: fixture\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n')
  expect(String((await reply)[0])).toContain('403 Forbidden')
  expect(setup.received).toHaveLength(0)
  expect(setup.spans).toHaveLength(0)
})
it('removes Connection-nominated headers as well as browser credentials', () => {
  expect(cleanHeaders({ connection: 'x-private', 'x-private': 'blocked', authorization: 'blocked', cookie: 'blocked', forwarded: 'blocked', 'x-forwarded-host': 'blocked', 'content-type': 'application/json' })).toEqual({ 'content-type': 'application/json' })
})
it('cancels an upstream response when its downstream client disconnects', async () => {
  const setup = await fixture()
  setup.gateway.removeAllListeners('request')
  let peerClosed: Promise<unknown> | undefined
  setup.gateway.on('request', (req, res) => {
    peerClosed = once(req.socket, 'close')
    res.writeHead(200)
    res.write('partial response')
  })
  const abort = new AbortController()
  const response = await fetch(`${setup.base}/v1/client/events`, { signal: abort.signal })
  expect(response.status).toBe(200)
  abort.abort()
  if (peerClosed === undefined) throw new TypeError('Missing upstream response')
  await peerClosed
})
it('bounds an unanswered upstream exchange by its absolute injected deadline', async () => {
  const setup = await fixture()
  setup.gateway.removeAllListeners('request')
  let peerClosed: Promise<unknown> | undefined
  setup.gateway.on('request', (req) => { peerClosed = once(req.socket, 'close') })
  expect((await fetch(`${setup.base}/v1/client/events`)).status).toBe(502)
  if (peerClosed === undefined) throw new TypeError('Missing upstream request')
  await peerClosed
})
it('cannot tunnel dot-segment escapes or malformed encoded paths past the client boundary', () => {
  for (const path of ['/v1/client/../claims', '/v1/client/%2e%2e/claims', '/v1/client/a%2f..%2f..%2fclaims', '/v1/client/%ZZ', '/v1/client/%00'])
    expect(clientRoute(path)).toBe(false)
  expect(clientRoute('/v1/client/agents/agent%2Fexample?cursor=opaque')).toBe(true)
})
it('handles upgrade socket errors while asynchronous admission is pending', async () => {
  let release!: (allowed: boolean) => void
  const setup = await fixture(() => new Promise<boolean>((resolve) => { release = resolve }))
  const accepted = new Promise<Duplex>((resolve) => setup.app.server.once('upgrade', (_req, socket) => resolve(socket)))
  const client = connect(setup.port, '127.0.0.1')
  client.on('error', () => undefined)
  cleanups.push(async () => { client.destroy() })
  await once(client, 'connect')
  client.write('GET /v1/client/collections/stream HTTP/1.1\r\nHost: fixture\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n')
  const socket = await accepted
  expect(socket.listenerCount('error')).toBeGreaterThan(0)
  const closed = new Promise<void>((resolve) => socket.once('close', () => resolve()))
  expect(() => socket.emit('error', Object.assign(new Error('synthetic reset'), { code: 'ECONNRESET' }))).not.toThrow()
  client.resetAndDestroy()
  await closed
  release(false)
  expect(socket.listenerCount('error')).toBe(0)
  expect(setup.received).toHaveLength(0)
  expect(setup.spans).toHaveLength(0)
})
it('retains upgrade error coverage through rejected socket shutdown', async () => {
  const setup = await fixture(() => false)
  const accepted = new Promise<Duplex>((resolve) => setup.app.server.once('upgrade', (_req, socket) => resolve(socket)))
  const client = connect({ port: setup.port, host: '127.0.0.1', allowHalfOpen: true })
  client.on('error', () => undefined)
  cleanups.push(async () => { client.destroy() })
  await once(client, 'connect')
  const reply = once(client, 'data')
  client.write('GET /v1/client/collections/stream HTTP/1.1\r\nHost: fixture\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n')
  const socket = await accepted
  expect(String((await reply)[0])).toContain('403 Forbidden')
  expect(socket.destroyed).toBe(false)
  expect(socket.listenerCount('error')).toBeGreaterThan(0)
  const closed = new Promise<void>((resolve) => socket.once('close', () => resolve()))
  expect(() => socket.emit('error', Object.assign(new Error('synthetic reset'), { code: 'ECONNRESET' }))).not.toThrow()
  client.resetAndDestroy()
  await closed
  expect(socket.listenerCount('error')).toBe(0)
  expect(setup.received).toHaveLength(0)
})
