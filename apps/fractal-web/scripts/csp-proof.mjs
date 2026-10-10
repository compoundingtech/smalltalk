#!/usr/bin/env node
/** Real app CSP proof across the app server, Vite preview and Vite development.
 * The negative control strips only the response header in a test-owned loopback proxy.
 * Browser routes supply finite, synthetic gateway frames and abort the remote image;
 * the route callback proves a request reached the network boundary without contacting it.
 * Run with CI=1 node apps/fractal-web/scripts/csp-proof.mjs.
 */
import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { randomBytes } from 'node:crypto'
import { once } from 'node:events'
import { mkdir, mkdtemp, rm } from 'node:fs/promises'
import { createServer as createHttpServer, request } from 'node:http'
import { fileURLToPath } from 'node:url'
import { Tracer } from 'effect'
import { build, createServer, loadConfigFromFile, preview } from 'vite'
import { createFractalWebServer } from '../server/core.mts'

const app = fileURLToPath(new URL('..', import.meta.url))
const scratchRoot = fileURLToPath(new URL('../../../tmp', import.meta.url))
await mkdir(scratchRoot, { recursive: true })
const scratch = await mkdtemp(`${scratchRoot}/csp-proof-`)
const session = `csp-${randomBytes(4).toString('hex')}`
const cleanups = []
const run = (args) => new Promise((resolve, reject) => {
  const child = spawn('playwright-cli', [`-s=${session}`, ...args], { cwd: scratch, stdio: ['ignore', 'pipe', 'inherit'] })
  let out = ''
  child.stdout.on('data', (chunk) => { out += chunk })
  const timer = setTimeout(() => { child.kill('SIGTERM'); reject(new Error('Browser command deadline exceeded')) }, 90_000)
  child.on('error', (error) => { clearTimeout(timer); reject(error) })
  child.on('exit', (code) => { clearTimeout(timer); code === 0 ? resolve(out) : reject(new Error(`Browser command exited ${code}: ${out}`)) })
})
const origin = (server) => `http://127.0.0.1:${server.address().port}`
const negativeControl = async (target) => {
  const proxy = createHttpServer((req, res) => {
    const upstream = request(new URL(req.url, target), { method: req.method, headers: { ...req.headers, host: new URL(target).host } }, (reply) => {
      const headers = { ...reply.headers }
      delete headers['content-security-policy']
      res.writeHead(reply.statusCode, headers)
      reply.pipe(res)
    })
    upstream.on('error', () => { res.writeHead(502); res.end() })
    req.pipe(upstream)
  })
  proxy.listen(0, '127.0.0.1')
  await once(proxy, 'listening')
  cleanups.push(async () => { proxy.closeAllConnections(); await new Promise((resolve) => proxy.close(resolve)) })
  return origin(proxy)
}
const proveHeaders = async (url, mode) => {
  const response = await fetch(url)
  const html = await response.text()
  const policy = response.headers.get('content-security-policy')
  assert(policy?.includes("img-src 'self' data: blob:"), `${mode}: HTML policy`)
  const head = await fetch(url, { method: 'HEAD' })
  assert.equal(head.headers.get('content-security-policy'), policy, `${mode}: HEAD must retain inline script hashes`)
  const cached = await fetch(url, { headers: { 'if-none-match': response.headers.get('etag') } })
  assert.equal(cached.status, 304, `${mode}: cached HTML`)
  assert.equal(cached.headers.get('content-security-policy'), policy, `${mode}: 304 must retain inline script hashes`)
  const asset = html.match(/<script\b[^>]*\bsrc="([^"]+)"/)?.[1]
  assert(asset, `${mode}: real module asset`)
  const assetResponse = await fetch(new URL(asset, url))
  await assetResponse.arrayBuffer()
  assert(assetResponse.headers.get('content-security-policy')?.includes("img-src 'self' data: blob:"), `${mode}: asset policy`)
  console.log(JSON.stringify({ mode, headers: 'PASS HTML, HEAD, 304 and module asset' }))
}
const prove = async (url, enforced, mode) => {
  const output = await run(['run-code', `async (page) => {
    const context = page.context()
    const testPage = await context.newPage()
    const remote = 'https://example.invalid/x.png'
    let imageAttempts = 0
    const errors = []
    testPage.on('pageerror', (error) => errors.push(String(error)))
    await testPage.addInitScript(() => {
      globalThis.__cspProofViolations = []
      globalThis.__cspProofImageOpens = []
      window.open = (...args) => { globalThis.__cspProofImageOpens.push(args); return null }
      document.addEventListener('securitypolicyviolation', (event) => globalThis.__cspProofViolations.push({
        directive: event.effectiveDirective, blockedURI: event.blockedURI, disposition: event.disposition,
      }))
    })
    const snapshot = { id: 'snapshot/proof', created_at: '2026-10-08T00:00:00Z', host_id: 'host/proof', projection_version: 'client-projection.v0', store_index: 1 }
    await testPage.route('**/v1/client/**', (route) => route.fulfill({ json: {
      api_version: 'st3.client.v0', snapshot, value: { capabilities: [], limits: { max_page_items: 100 } },
    } }))
    await testPage.route('**/otlp/**', (route) => route.fulfill({ status: 204 }))
    await testPage.routeWebSocket('**/v1/client/collections/stream**', (socket) => {
      socket.onMessage((message) => {
        const command = JSON.parse(String(message))
        if (command.kind !== 'subscribe') return
        if (command.collection === 'conversation') {
          socket.send(JSON.stringify({ kind: 'conversation', id: command.id, collection: 'conversation',
            session_id: 'session/proof', replace: true, has_more: false, items: [{
              id: 'timeline-entry/image', sequence: 1, revision: 1, type: 'content', role: 'assistant',
              final: true, timestamp: snapshot.created_at, body: { text: '![Preview](' + remote + ')', media_type: 'text/markdown' },
            }] }))
        } else {
          socket.send(JSON.stringify({ kind: 'snapshot', id: command.id,
            collection: command.collection, has_more: false, items: [], order: [], snapshot }))
        }
      })
    })
    await testPage.route(remote, async (route) => { imageAttempts++; await route.abort() })
    const response = await testPage.goto(${JSON.stringify(new URL('/w/agent/proof', url).href)})
    await testPage.getByTestId('live-agent-workspace').waitFor({ timeout: 60000 })
    const placeholder = testPage.getByTestId('deferred-image')
    await placeholder.waitFor({ timeout: 60000 })
    const hostShown = (await placeholder.textContent()).includes('example.invalid')
    const remoteImagesBefore = await testPage.locator('img[src="' + remote + '"]').count()
    await placeholder.getByRole('button', { name: 'Open image · example.invalid', exact: true }).click()
    const imageBinding = {
      hostShown, remoteImagesBefore, remoteImagesAfter: await testPage.locator('img[src="' + remote + '"]').count(),
      imageAttempts, opens: await testPage.evaluate(() => [...globalThis.__cspProofImageOpens]),
    }
    const bootViolations = await testPage.evaluate(() => [...globalThis.__cspProofViolations])
    await testPage.evaluate((src) => new Promise((resolve) => {
      const image = document.createElement('img')
      image.onload = () => resolve('load')
      image.onerror = () => resolve('error')
      image.src = src
      document.body.append(image)
    }), remote)
    // A task boundary lets securitypolicyviolation events dispatch after the image error.
    await testPage.evaluate(() => new Promise((resolve) => setTimeout(resolve, 0)))
    const violations = await testPage.evaluate(() => [...globalThis.__cspProofViolations])
    const result = { mode: ${JSON.stringify(mode)}, enforced: ${enforced}, rendered: true,
      policy: response.headers()['content-security-policy'] ?? null, imageBinding, imageAttempts, bootViolations, violations, errors }
    await testPage.close()
    return 'CSP_PROOF ' + JSON.stringify(result)
  }`])
  const line = output.split('\n').find((text) => text.includes('CSP_PROOF '))
  assert(line, `No browser receipt: ${output}`)
  const quoted = line.trim()
  const result = JSON.parse(JSON.parse(quoted).slice('CSP_PROOF '.length))
  console.log(JSON.stringify(result))
  assert.equal(result.rendered, true, 'the real app must render')
  assert.deepEqual(result.errors, [], 'app must boot without runtime errors')
  assert.deepEqual(result.bootViolations, [], 'app must boot without CSP violations')
  assert.deepEqual(result.imageBinding, {
    hostShown: true, remoteImagesBefore: 0, remoteImagesAfter: 0, imageAttempts: 0,
    opens: [['https://example.invalid/x.png', '_blank', 'noopener,noreferrer']],
  }, 'Transcript image consent must hand off without an inline image request')
  if (enforced) {
    assert(result.policy?.includes("img-src 'self' data: blob:"), 'response must carry the image policy')
    await proveHeaders(url, mode)
    assert.equal(result.imageAttempts, 0, 'remote image must not reach the network boundary')
    assert.deepEqual(result.violations, [{ directive: 'img-src', blockedURI: 'https://example.invalid/x.png', disposition: 'enforce' }])
  } else {
    assert.equal(result.policy, null)
    assert.equal(result.imageAttempts, 1, 'negative control must attempt the image request')
    assert.deepEqual(result.violations, [])
  }
}
try {
  const loaded = await loadConfigFromFile({ command: 'build', mode: 'production' }, `${app}/vite.config.ts`, app)
  assert(loaded, 'Vite configuration must load')
  const config = { ...loaded.config, configFile: false, root: `${app}/src/web`, logLevel: 'warn',
    build: { ...loaded.config.build, outDir: `${scratch}/dist`, emptyOutDir: true },
    // Never inherit a paired live gateway into a synthetic browser proof.
    plugins: loaded.config.plugins.flat().filter((plugin) => plugin?.name !== 'wf:shared-client-gateway'),
  }
  await build(config)
  const served = createFractalWebServer({ dist: `${scratch}/dist`, host: '127.0.0.1', port: 0,
    admit: () => true, tracer: Tracer.make({ span: (options) => new Tracer.NativeSpan(options) }),
    gateway: { socketPath: `${scratch}/unused.sock`, host: 'gateway.invalid', authorization: ['Bearer', 'synthetic'].join(' '), timeoutMs: 1000 },
    identity: { deploymentId: 'csp-proof', buildIdentity: { baseVersion: '1.0.0', displayVersion: '1.0.0-test', machineVersion: '1.0.0+test', sourceKind: 'local', dirty: false } },
  })
  served.listen()
  await once(served.server, 'listening')
  cleanups.push(() => served.close())
  await run(['open', 'about:blank'])
  await prove(await negativeControl(origin(served.server)), false, 'production-negative-control')
  await prove(origin(served.server), true, 'production')
  const previewServer = await preview({ ...config, preview: { host: '127.0.0.1', port: 0, strictPort: false } })
  cleanups.push(() => previewServer.close())
  await prove(origin(previewServer.httpServer), true, 'preview')
  // Vite build sets NODE_ENV; do not run the development phase as production.
  process.env.NODE_ENV = 'development'
  const devLoaded = await loadConfigFromFile({ command: 'serve', mode: 'development' }, `${app}/vite.config.ts`, app)
  const dev = await createServer({ ...devLoaded.config, configFile: false, root: `${app}/src/web`, logLevel: 'warn',
    // Isolate optimizer state from concurrent app servers and other proofs.
    cacheDir: `${scratch}/vite-cache`,
    // Discover the kit Markdown imports together, before HMR-disabled navigation can see outdated dep URLs.
    optimizeDeps: { ...devLoaded.config.optimizeDeps, include: [
      ...(devLoaded.config.optimizeDeps?.include ?? []),
      'react', 'react-dom/client', 'react/jsx-runtime', 'react/jsx-dev-runtime', '@assistant-ui/react',
      'react-aria-components', 'react-markdown', 'remark-gfm', 'refractor/core',
      'refractor/typescript', 'refractor/tsx', 'refractor/javascript', 'refractor/json',
      'refractor/bash', 'refractor/diff', 'refractor/rust', 'refractor/nix', 'refractor/python',
      'refractor/yaml', 'refractor/markdown', 'refractor/css',
    ] },
    // As in the existing browser proofs, never inherit a paired live gateway.
    plugins: devLoaded.config.plugins.flat().filter((plugin) => plugin?.name !== 'wf:shared-client-gateway'),
    server: { ...devLoaded.config.server, host: '127.0.0.1', port: 0, strictPort: false, hmr: false, preTransformRequests: false },
  })
  await dev.listen()
  cleanups.push(() => dev.close())
  await prove(origin(dev.httpServer), true, 'development')
  console.log('PASS CSP blocks remote images; negative control attempts the request; real app renders in production, preview and development')
} finally {
  try { await run(['close']) } finally {
    while (cleanups.length) await cleanups.pop()()
    await rm(scratch, { recursive: true, force: true })
  }
}
