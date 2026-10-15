#!/usr/bin/env node
/** Real StyleX rules: late success, timeout then success, and HTTP failure then explicit reload. */
import { mkdir, writeFile } from 'node:fs/promises'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { createServer, loadConfigFromFile } from 'vite'
const app = fileURLToPath(new URL('..', import.meta.url))
const evidence = process.env.WF_PROOF_DIR
if (!evidence) throw new Error('WF_PROOF_DIR is required')
await mkdir(evidence, { recursive: true })
const loaded = await loadConfigFromFile({ command: 'serve', mode: 'development' }, `${app}/vite.config.ts`, app)
let phase = { delay: 1100, fail: false }
const server = await createServer({ ...loaded.config, configFile: false, root: `${app}/src/web`,
  plugins: [{ name: 'proof:stylesheet-delivery', enforce: 'pre', configureServer(vite) { vite.middlewares.use((req, res, next) => {
    if (!req.url?.startsWith('/virtual:stylex.css')) return next()
    const current = phase
    if (current.fail) { res.writeHead(500, { 'content-type': 'text/css' }); res.end(''); return }
    setTimeout(next, current.delay)
  }) } }, ...loaded.config.plugins.flat().filter(plugin => plugin?.name !== 'wf:shared-client-gateway')],
  optimizeDeps: { noDiscovery: true, include: ['@stylexjs/stylex'] },
  server: { ...loaded.config.server, host: '127.0.0.1', port: 0, strictPort: false, hmr: false, preTransformRequests: false },
})
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE ? pathToFileURL(process.env.PLAYWRIGHT_MODULE).href : 'playwright')
const browser = await chromium.launch(process.env.CHROMIUM_PATH ? { executablePath: process.env.CHROMIUM_PATH } : {})
const results = []
try {
  await server.listen()
  await server.transformRequest('/stylexLoad.browser.ts')
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`
  for (const condition of ['late', 'timeout', 'error']) {
    phase = { delay: condition === 'timeout' ? 5500 : 1100, fail: condition === 'error' }
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 }, colorScheme: 'dark' })
    await page.route('**/main.tsx*', route => route.fulfill({ contentType: 'text/javascript', body: '' }))
    await page.route('**/*virtual:stylex:runtime*', route => route.fulfill({ contentType: 'text/javascript', body: '' }))
    await page.route('**/v1/client/**', route => route.abort())
    await page.goto(origin, { waitUntil: 'commit' })
    const mediaBeforeLoad = await page.locator('link[data-wf-stylex-bootstrap]').getAttribute('media')
    let recoveryActionSeen = false
    await Promise.all([
      page.evaluate(async () => { await import('/stylexLoad.browser.ts') }),
      condition === 'late' ? Promise.resolve() : page.getByRole('button', { name: 'Reload workspace' }).waitFor({ timeout: 10_000 }).then(async () => {
        recoveryActionSeen = true
        await page.screenshot({ path: `${evidence}/stylex-${condition}-recovery.png` })
      }),
    ])
    if (condition === 'error') {
      phase = { delay: 0, fail: false }
      await Promise.all([page.waitForNavigation(), page.getByRole('button', { name: 'Reload workspace' }).click()])
      await page.evaluate(async () => { await import('/stylexLoad.browser.ts') })
    }
    await page.waitForFunction(() => document.querySelector('link[data-wf-stylex-bootstrap]').media === 'all' && getComputedStyle(document.querySelector('[data-style-proof]')).color === 'rgb(10, 80, 170)', undefined, { timeout: 15_000 })
    const facts = await page.evaluate(() => ({ media: document.querySelector('link[data-wf-stylex-bootstrap]').media, styledColor: getComputedStyle(document.querySelector('[data-style-proof]')).color, noticeRemoved: document.querySelector('[data-wf-style-failure]') === null, skeletonTheme: getComputedStyle(document.querySelector('[data-wf-shell-skeleton]')).backgroundColor }))
    await page.screenshot({ path: `${evidence}/stylex-${condition}-styled.png` })
    results.push({ condition, mediaBeforeLoad, recoveryActionSeen, ...facts })
    if (mediaBeforeLoad !== 'print' || facts.media !== 'all' || !facts.noticeRemoved || facts.styledColor !== 'rgb(10, 80, 170)') throw new Error(JSON.stringify(results.at(-1)))
    await page.close()
  }
  await writeFile(`${evidence}/stylex-load.json`, JSON.stringify({ results }, null, 2))
  console.log(JSON.stringify({ status: 'passed', results }))
} finally { await browser.close(); await server.close() }
