#!/usr/bin/env node
/** Fresh-document responseEnd -> themed-skeleton rAF/screencast proof.
 * Own Vite server; real bootstrap; app/runtime modules held so the skeleton remains observable.
 * A synthetic 1100ms CSS response delay exercises slow stylesheet delivery without
 * fabricating stylesheet bytes. The control restores the original blocking stylesheet link.
 */
import { mkdir, writeFile } from 'node:fs/promises'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { createServer, loadConfigFromFile } from 'vite'
const app = fileURLToPath(new URL('..', import.meta.url))
const evidence = process.env.WF_PROOF_DIR
if (!evidence) throw new Error('WF_PROOF_DIR is required')
await mkdir(evidence, { recursive: true })
const control = process.argv.includes('--control')
const loaded = await loadConfigFromFile({ command: 'serve', mode: 'development' }, `${app}/vite.config.ts`, app)
const server = await createServer({ ...loaded.config, configFile: false, root: `${app}/src/web`,
  plugins: [{
    name: 'proof:slow-real-stylex-css', enforce: 'pre', configureServer(vite) { vite.middlewares.use((req, _res, next) => { if (req.url?.startsWith('/virtual:stylex.css')) setTimeout(next, 1100); else next() }) },
    transformIndexHtml: { order: 'post', handler(html) { return control ? html.replace(/<link\b[^>]*href="\/virtual:stylex\.css"[^>]*>/g, link => link.replace(/\smedia="print"/, '')) : html } },
  }, ...loaded.config.plugins.flat().filter(plugin => plugin?.name !== 'wf:shared-client-gateway')], optimizeDeps: { entries: ['index.html'] },
  server: { ...loaded.config.server, host: '127.0.0.1', port: 0, strictPort: false, hmr: false },
})
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE ? pathToFileURL(process.env.PLAYWRIGHT_MODULE).href : 'playwright')
const browser = await chromium.launch({ ...(process.env.CHROMIUM_PATH ? { executablePath: process.env.CHROMIUM_PATH } : {}), args: ['--disable-background-timer-throttling'] })
const results = []
try {
  await server.listen()
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`
  const context = await browser.newContext({ viewport: { width: 1440, height: 1000 }, colorScheme: 'dark' })
  for (const cache of ['disabled', 'warmed']) for (let index = 0; index < 10; index++) {
    const page = await context.newPage()
    await page.route('**/main.tsx*', route => route.fulfill({ contentType: 'text/javascript', body: '' }))
    await page.route('**/*virtual:stylex:runtime*', route => route.fulfill({ contentType: 'text/javascript', body: '' }))
    await page.route('**/v1/client/**', route => route.abort())
    await page.addInitScript(() => {
      localStorage.setItem('wf.ui@1', JSON.stringify({ version: 1, entries: { 'round2.scheme': { version: 1, value: 'dark' } } }))
      window.__gatePaint = { firstSkeleton: null }
      const tick = () => {
        const skeleton = document.querySelector('[data-wf-shell-skeleton]')
        if (skeleton && window.__gatePaint.firstSkeleton === null) window.__gatePaint.firstSkeleton = { t: performance.now(), canvas: getComputedStyle(skeleton).backgroundColor, sidebar: getComputedStyle(document.querySelector('[data-wf-sidebar-skeleton]')).backgroundColor }
        if (window.__gatePaint.firstSkeleton === null) requestAnimationFrame(tick)
      }
      requestAnimationFrame(tick)
    })
    await page.goto(origin, { waitUntil: 'domcontentloaded' })
    await page.goto('about:blank')
    const cdp = await context.newCDPSession(page)
    await cdp.send('Network.enable')
    await cdp.send('Network.setCacheDisabled', { cacheDisabled: cache === 'disabled' })
    if (cache === 'disabled') await cdp.send('Network.clearBrowserCache')
    const frames = [], trace = []
    cdp.on('Page.screencastFrame', frame => { frames.push(frame); void cdp.send('Page.screencastFrameAck', { sessionId: frame.sessionId }) })
    if (index === 0) { cdp.on('Tracing.dataCollected', event => trace.push(...event.value)); await cdp.send('Tracing.start', { categories: 'devtools.timeline,loading,blink.user_timing', transferMode: 'ReportEvents' }) }
    await cdp.send('Page.startScreencast', { format: 'png', maxWidth: 1440, maxHeight: 1000, everyNthFrame: 1 })
    await page.goto(origin, { waitUntil: 'domcontentloaded', timeout: 60_000 })
    // Retain each document for 1800ms after DOMContentLoaded to avoid overlapping
    // unfinished stylesheets in the next fresh-document sample.
    await page.waitForTimeout(1800)
    await page.waitForFunction(() => window.__gatePaint?.firstSkeleton !== null && performance.getEntriesByType('paint').length > 0)
    await cdp.send('Page.stopScreencast')
    if (index === 0) { const done = new Promise(resolve => cdp.once('Tracing.tracingComplete', resolve)); await cdp.send('Tracing.end'); await done; await writeFile(`${evidence}/${control ? 'old' : 'fixed'}-${cache}-trace.json`, JSON.stringify({ traceEvents: trace })) }
    const facts = await page.evaluate(() => ({ timeOrigin: performance.timeOrigin, nav: performance.getEntriesByType('navigation')[0].toJSON(), paints: performance.getEntriesByType('paint').map(entry => entry.toJSON()), resources: performance.getEntriesByType('resource').map(entry => entry.toJSON()), skeleton: window.__gatePaint.firstSkeleton, links: [...document.querySelectorAll('link[rel="stylesheet"]')].map(link => ({ href: link.href, media: link.media })) }))
    const valid = frames.filter(frame => frame.metadata.timestamp * 1000 - facts.timeOrigin >= facts.nav.responseStart)
    if (index === 0 && valid[0]) await writeFile(`${evidence}/${control ? 'old' : 'fixed'}-${cache}-first.png`, Buffer.from(valid[0].data, 'base64'))
    const skeletonAfterResponseEnd = facts.skeleton.t - facts.nav.responseEnd
    const passes = skeletonAfterResponseEnd <= 300 && facts.skeleton.canvas === 'rgb(10, 10, 10)' && facts.skeleton.sidebar === 'rgb(0, 0, 0)'
    results.push({ cache, iteration: index + 1, control, passes, skeletonAfterResponseEnd, postResponseFrames: valid.length, ...facts })
    await cdp.detach(); await page.close()
  }
  const summary = ['disabled', 'warmed'].map(cache => { const runs = results.filter(run => run.cache === cache), times = runs.map(run => run.skeletonAfterResponseEnd).sort((a, b) => a - b); return { cache, count: runs.length, within300: runs.filter(run => run.passes).length, min: times[0], median: (times[4] + times[5]) / 2, p95: times[9], max: times[9] } })
  await writeFile(`${evidence}/${control ? 'old-control' : 'fixed'}.json`, JSON.stringify({ condition: 'Own loopback Vite; real CSS response delayed 1100ms; modules held; responseEnd-to-skeleton rAF and screencast proof', summary, results }, null, 2))
  console.log(JSON.stringify({ control, summary }))
  if (results.some(run => run.passes === control)) throw new Error('Skeleton paint control/acceptance failed')
} finally { await browser.close(); await server.close() }
