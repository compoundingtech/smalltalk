import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createServer } from 'node:http'
import { createIsolatedKitWorker } from './kit-play-isolation.mjs'
import { strictStoryGuard } from './details-strict-story-guard.mjs'

const pageOptions = { viewport: { width: 440, height: 900 }, colorScheme: 'dark' }
const storyDocument = failed => `<!doctype html><meta charset="utf-8"><title>Story fixture</title>
<script>
  const listeners = new Map()
  window.__STORYBOOK_ADDONS_CHANNEL__ = {
    on(name, listener) {
      listeners.set(name, listener)
      // Emit only after the real strict guard has subscribed to all lifecycle
      // events. No wall-clock sleep or replacement for the guard is involved.
      if (name === 'storyErrored') queueMicrotask(() => {
        if (${failed}) listeners.get('playFunctionThrewException')({ message: 'deliberately broken play' })
        listeners.get('storyFinished')({ storyId: '${failed ? 'kit--broken' : 'kit--success'}', status: '${failed ? 'error' : 'success'}', reporters: [] })
      })
    },
  }
</script>`

const seedState = async page => page.evaluate(async () => {
  window.__previousKitCase = 'must not survive'
  localStorage.setItem('previous-case', 'local')
  sessionStorage.setItem('previous-case', 'session')
  document.cookie = 'previous-case=cookie; path=/; SameSite=Lax'
  await new Promise((resolve, reject) => {
    const request = indexedDB.open('previous-case', 1)
    request.onupgradeneeded = () => request.result.createObjectStore('values')
    request.onerror = () => reject(request.error)
    request.onsuccess = () => {
      const database = request.result
      const transaction = database.transaction('values', 'readwrite')
      transaction.objectStore('values').put('persisted', 'key')
      transaction.oncomplete = () => { database.close(); resolve() }
      transaction.onabort = () => { database.close(); reject(transaction.error) }
      transaction.onerror = () => { database.close(); reject(transaction.error) }
    }
  })
  const cache = await caches.open('previous-case')
  await cache.put('/previous-case', new Response('persisted'))
})

const readState = async page => page.evaluate(async () => ({
  global: window.__previousKitCase ?? null,
  local: localStorage.getItem('previous-case'),
  session: sessionStorage.getItem('previous-case'),
  cookie: document.cookie,
  databases: (await indexedDB.databases()).map(database => database.name),
  caches: await caches.keys(),
}))
const emptyState = { global: null, local: null, session: null, cookie: '', databases: [], caches: [] }

test('reused Chromium worker isolates cases and rejects contamination and broken plays', { timeout: 30000 }, async t => {
  const servers = []
  let browser, worker, workerB, assetRequests = 0
  t.after(async () => {
    // Run every cleanup even when another cleanup fails; node:test preserves
    // these hook errors alongside the original assertion or protocol failure.
    const results = await Promise.allSettled([
      (async () => { await worker?.close() })(),
      (async () => { await workerB?.close() })(),
      ...servers.map(server => new Promise((resolve, reject) => {
        server.close(error => error ? reject(error) : resolve())
        server.closeAllConnections()
      })),
    ])
    try {
      await browser?.close()
    } catch (error) {
      results.push({ status: 'rejected', reason: error })
    }
    const failures = results.filter(result => result.status === 'rejected')
    if (failures.length) throw new AggregateError(failures.map(result => result.reason), 'Browser isolation test cleanup failed')
  })
  const serve = async () => {
    const server = createServer((request, response) => {
      const { pathname } = new URL(request.url, 'http://localhost')
      if (pathname === '/asset') {
        assetRequests++
        response.writeHead(200, { 'Content-Type': 'text/plain', 'Cache-Control': 'public, max-age=3600' }).end('immutable asset')
      } else if (pathname === '/service-worker.js') {
        response.writeHead(200, { 'Content-Type': 'text/javascript', 'Cache-Control': 'no-store' }).end(`
          self.addEventListener('install', event => event.waitUntil(self.skipWaiting()))
          self.addEventListener('activate', event => event.waitUntil(self.clients.claim()))
          self.addEventListener('fetch', event => {
            if (new URL(event.request.url).pathname === '/sw-proof') {
              event.respondWith(Promise.resolve(new Response('controlled by previous case')))
            }
          })
        `)
      } else {
        response.writeHead(200, { 'Content-Type': 'text/html' }).end(pathname.endsWith('/iframe.html') ? storyDocument(pathname !== '/success/iframe.html') : '<!doctype html><title>Isolation fixture</title>')
      }
    })
    servers.push(server)
    await new Promise((resolve, reject) => {
      server.once('error', reject)
      server.listen(0, '127.0.0.1', resolve)
    })
    return `http://127.0.0.1:${server.address().port}`
  }
  const base = await serve(), otherOrigin = await serve()
  const { chromium } = await import(process.env.PLAYWRIGHT_MODULE ?? 'playwright')
  browser = await chromium.launch(process.env.CHROMIUM_PATH ? { executablePath: process.env.CHROMIUM_PATH } : {})
  worker = await createIsolatedKitWorker(browser, base)
  await worker.reset()
  const control = worker.context.pages()[0]
  const previous = await worker.newPage(pageOptions)
  await previous.goto(base)
  await seedState(previous)
  assert.deepEqual(await readState(previous), {
    global: 'must not survive', local: 'local', session: 'session', cookie: 'previous-case=cookie',
    databases: ['previous-case'], caches: ['previous-case'],
  })
  assert.equal(await previous.evaluate(() => fetch('/asset').then(response => response.text())), 'immutable asset')
  assert.equal(assetRequests, 1)
  await previous.evaluate(async () => {
    const controlled = new Promise(resolve => {
      if (navigator.serviceWorker.controller) resolve()
      else navigator.serviceWorker.addEventListener('controllerchange', resolve, { once: true })
    })
    await navigator.serviceWorker.register('/service-worker.js')
    await navigator.serviceWorker.ready
    await controlled
  })
  assert.equal(await previous.evaluate(() => fetch('/sw-proof').then(response => response.text())), 'controlled by previous case')
  await worker.context.grantPermissions(['geolocation'], { origin: base })
  assert.equal(await previous.evaluate(async () => (await navigator.permissions.query({ name: 'geolocation' })).state), 'granted')
  const popupEvent = previous.waitForEvent('popup')
  await previous.evaluate(url => { window.open(url) }, otherOrigin)
  const popup = await popupEvent
  await popup.waitForLoadState('load')
  await seedState(popup)
  assert.deepEqual(await readState(popup), {
    global: 'must not survive', local: 'local', session: 'session', cookie: 'previous-case=cookie',
    databases: ['previous-case'], caches: ['previous-case'],
  })
  // These request URLs must never be passed to CDP as HTTP storage origins.
  const opaque = await worker.newPage(pageOptions)
  await opaque.goto('data:text/html,<title>Opaque case</title>')
  const blobURL = await previous.evaluate(() => URL.createObjectURL(new Blob(['<title>Blob case</title>'], { type: 'text/html' })))
  const blob = await worker.newPage(pageOptions)
  await blob.goto(blobURL)

  // CDP storage operations must target the worker's own browser context. The
  // same HTTP origin in another live worker must remain entirely untouched.
  workerB = await createIsolatedKitWorker(browser, base)
  const pageB = await workerB.newPage(pageOptions)
  await pageB.goto(base)
  assert.deepEqual(await readState(pageB), emptyState)
  await seedState(pageB)
  await workerB.reset()
  assert.ok(pageB.isClosed())
  assert.deepEqual(await readState(previous), {
    global: 'must not survive', local: 'local', session: 'session', cookie: 'previous-case=cookie',
    databases: ['previous-case'], caches: ['previous-case'],
  })
  assert.deepEqual(await readState(popup), {
    global: 'must not survive', local: 'local', session: 'session', cookie: 'previous-case=cookie',
    databases: ['previous-case'], caches: ['previous-case'],
  })
  assert.equal(await previous.evaluate(() => fetch('/sw-proof').then(response => response.text())), 'controlled by previous case')
  const cleanB = await workerB.newPage(pageOptions)
  await cleanB.goto(base)
  assert.deepEqual(await readState(cleanB), emptyState)

  const { resetMilliseconds } = await worker.reset()
  assert.ok(Number.isFinite(resetMilliseconds) && resetMilliseconds >= 0)
  assert.ok(previous.isClosed() && popup.isClosed() && opaque.isClosed() && blob.isClosed())
  assert.deepEqual(worker.context.pages(), [control])
  assert.equal(control.url(), 'about:blank')
  assert.deepEqual(await worker.context.cookies(), [])
  await worker.audit()

  const next = await worker.newPage({ viewport: { width: 1440, height: 900 }, colorScheme: 'light' })
  assert.notEqual(next, previous)
  await next.goto(base)
  assert.deepEqual(await readState(next), emptyState)
  assert.match(await next.evaluate(() => navigator.userAgent), / StorybookTestRunner$/)
  assert.equal(await next.evaluate(() => navigator.serviceWorker.controller === null), true)
  assert.equal(await next.evaluate(async () => (await navigator.serviceWorker.getRegistrations()).length), 0)
  assert.notEqual(await next.evaluate(() => fetch('/sw-proof').then(response => response.text())), 'controlled by previous case')
  assert.deepEqual(next.viewportSize(), { width: 1440, height: 900 })
  assert.equal(await next.evaluate(() => matchMedia('(prefers-color-scheme: light)').matches), true)
  assert.equal(await next.evaluate(async () => (await navigator.permissions.query({ name: 'geolocation' })).state), 'prompt')
  assert.equal(await next.evaluate(() => fetch('/asset').then(response => response.text())), 'immutable asset')
  assert.equal(assetRequests, 1, 'HTTP asset cache survives storage reset')
  const nextOtherOrigin = await worker.newPage(pageOptions)
  await nextOtherOrigin.goto(otherOrigin)
  assert.deepEqual(await readState(nextOtherOrigin), emptyState)
  assert.equal(await nextOtherOrigin.evaluate(() => matchMedia('(prefers-color-scheme: dark)').matches), true)

  // Deliberately contaminate after a successful reset: the public audit must
  // observe real quota-backed storage, not merely trust reset's completion.
  await seedState(nextOtherOrigin)
  await worker.context.clearCookies()
  await assert.rejects(() => worker.audit(), /Kit isolation audit: residual storage/)
  await worker.context.addCookies([{ name: 'negative-control', value: 'present', url: base }])
  await assert.rejects(() => worker.audit(), /Kit isolation audit: .* residual cookies/)
  await worker.reset()

  // A case may legitimately write storage and navigate again. Only its first
  // HTTP(S) top-level document must start empty, matching fresh-context cases.
  const navigating = await worker.newPage(pageOptions)
  await navigating.goto(base)
  await navigating.evaluate(() => {
    localStorage.setItem('within-case', 'local')
    sessionStorage.setItem('within-case', 'session')
  })
  await navigating.goto(`${base}/another-document`)
  assert.deepEqual(await navigating.evaluate(() => ({
    local: localStorage.getItem('within-case'),
    session: sessionStorage.getItem('within-case'),
  })), { local: 'local', session: 'session' })
  await worker.reset()

  // localStorage may not contribute to CDP quota usage. The real strict
  // guard wraps the helper's goto, whose first-document audit must reject.
  const writer = await worker.newPage(pageOptions)
  await writer.goto(base)
  await writer.evaluate(() => localStorage.setItem('local-only-negative-control', 'present'))
  assert.deepEqual(await worker.context.cookies(), [])
  const contaminated = await worker.newPage(pageOptions)
  const contaminatedGuard = await strictStoryGuard(contaminated, base)
  await assert.rejects(() => contaminated.goto(`${base}/success/iframe.html?id=kit--success`, { waitUntil: 'domcontentloaded' }), error => {
    // The helper rejects before strictStoryGuard can accept storyFinished.
    assert.match(error.message, /Kit isolation document-start audit: residual Web Storage/)
    return true
  })
  assert.equal(contaminatedGuard.records.length, 0)
  await worker.reset()
  const cleanStory = await worker.newPage(pageOptions)
  const cleanGuard = await strictStoryGuard(cleanStory, base)
  await cleanStory.goto(`${base}/success/iframe.html?id=kit--success`, { waitUntil: 'domcontentloaded' })
  await cleanGuard.check()
  assert.ok(cleanGuard.records.length > 0)
  await worker.reset()

  const broken = await worker.newPage(pageOptions)
  const guard = await strictStoryGuard(broken, base)
  await assert.rejects(() => broken.goto(`${base}/iframe.html?id=kit--broken`, { waitUntil: 'domcontentloaded' }), error => {
    assert.match(error.message, /strict-storybook-readiness/)
    assert.match(error.message, /deliberately broken play/)
    assert.match(error.message, /"status":"error"/)
    return true
  })
  assert.equal(guard.records.length, 0, 'broken play must not produce successful strict evidence')
  await worker.reset()
})
