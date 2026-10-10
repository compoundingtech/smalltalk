#!/usr/bin/env node
/**
 * Dev-server proof: after StyleX HMR saves, a lost HMR socket does not flood the page with
 * "send was called before connect" errors. Serves a synthetic StyleX fixture with the app's own
 * Vite config on an ephemeral loopback port (no gateway), saves it atomically (rename) and in
 * place, answers four virtual StyleX stylesheet fetches with 410, closes the HMR socket and
 * raises one console error and one unhandled rejection while it is down.
 *
 * Vite's browser console forwarder before 8.0.14 (vitejs/vite#22407, fixed by #22450) sends
 * every unhandled rejection to the server and leaves its own failed send unhandled, so each
 * rejection re-enters the listener. The app config turns `server.forwardConsole` off.
 *
 * `--without-fix` is the negative control: it forces `forwardConsole: true` and asserts the storm.
 * The control needs an affected Vite; select one with VITE_MODULE (default: the app's `vite`).
 * PLAYWRIGHT_MODULE: path or specifier of the playwright module (default 'playwright').
 * CHROMIUM_PATH: optional browser executable.
 *
 *   node apps/fractal-web/scripts/hmr-disconnect-proof.mjs [--without-fix]
 */
import { randomBytes } from 'node:crypto'
import { readFile, rename, rm, writeFile } from 'node:fs/promises'
import { fileURLToPath, pathToFileURL } from 'node:url'

const app = fileURLToPath(new URL('..', import.meta.url))
const withoutFix = process.argv.includes('--without-fix')
const viteModule = process.env.VITE_MODULE ?? 'vite'
const { createServer, loadConfigFromFile, version } = await import(viteModule.startsWith('/') ? pathToFileURL(viteModule).href : viteModule)
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE ?? 'playwright')
const marker = 'send was called before connect'
// One rejection while disconnected; a recursing forwarder turns it into thousands within the window.
const floodThreshold = 100

const name = `hmr-disconnect-proof-${randomBytes(4).toString('hex')}`
const fixtureModule = `${app}/src/web/${name}.ts`
const fixtureHtml = `${app}/src/web/${name}.html`
const fixtureSource = (weight) => `import * as stylex from '@stylexjs/stylex'
const styles = stylex.create({ label: { fontWeight: ${weight}, color: 'rgb(20, 20, 20)' } })
const node = document.getElementById('proof') ?? document.body.appendChild(Object.assign(document.createElement('p'), { id: 'proof', textContent: 'HMR proof' }))
node.className = stylex.props(styles.label).className ?? ''
if (import.meta.hot) import.meta.hot.accept()
`
await writeFile(fixtureModule, fixtureSource(400))
await writeFile(fixtureHtml, `<!doctype html><html><head><meta charset="utf-8"><title>HMR proof</title></head><body><script type="module" src="./${name}.ts"></script></body></html>\n`)

const loaded = await loadConfigFromFile({ command: 'serve', mode: 'development' }, `${app}/vite.config.ts`, app)
const server = await createServer({
  ...loaded.config,
  configFile: false,
  root: `${app}/src/web`,
  optimizeDeps: { ...loaded.config.optimizeDeps, entries: [`${name}.html`] },
  // The fixture owns its data; never connect to an inherited live gateway.
  plugins: loaded.config.plugins.flat().filter((plugin) => plugin?.name !== 'wf:shared-client-gateway'),
  server: { ...loaded.config.server, host: '127.0.0.1', port: 0, strictPort: false, ...(withoutFix ? { forwardConsole: true } : {}) },
})
const browser = await chromium.launch(process.env.CHROMIUM_PATH ? { executablePath: process.env.CHROMIUM_PATH } : {})
try {
  await server.listen()
  const { port } = server.httpServer.address()
  const page = await browser.newPage()
  const counts = { sendBeforeConnect: 0, stylesheet410: 0 }
  const count = (text) => { if (text.includes(marker)) counts.sendBeforeConnect++ }
  page.on('console', (message) => count(message.text()))
  page.on('pageerror', (error) => count(String(error)))
  await page.addInitScript(() => {
    const Native = window.WebSocket
    window.__hmrSockets = []
    window.WebSocket = class extends Native {
      constructor(...args) {
        super(...args)
        if (args[1] === 'vite-hmr') window.__hmrSockets.push(this)
      }
    }
  })
  await page.goto(`http://127.0.0.1:${port}/${name}.html`)
  await page.waitForFunction(() => window.__hmrSockets.some((socket) => socket.readyState === WebSocket.OPEN), undefined, { timeout: 45_000 })
  await page.evaluate(async () => {
    const { createHotContext } = await import('/@vite/client')
    createHotContext('/hmr-disconnect-proof').on('vite:afterUpdate', () => { window.__hmrUpdates = (window.__hmrUpdates ?? 0) + 1 })
  })
  const saves = [600, 400, 500, 700]
  for (const [index, weight] of saves.entries()) {
    if (index % 2 === 0) {
      await writeFile(`${fixtureModule}.save`, fixtureSource(weight))
      await rename(`${fixtureModule}.save`, fixtureModule)
    } else await writeFile(fixtureModule, fixtureSource(weight))
    await page.waitForFunction((n) => (window.__hmrUpdates ?? 0) >= n, index + 1, { timeout: 20_000 })
      .catch(async (error) => { throw new Error(`HMR save ${index + 1} did not apply (updates ${await page.evaluate(() => window.__hmrUpdates ?? 0)})`, { cause: error }) })
    // Separate saves so the watcher reports each one instead of coalescing them.
    await page.waitForTimeout(300)
  }
  // The StyleX dev runtime refetches the compiled stylesheet after each update.
  const styled = await page.waitForFunction(() => getComputedStyle(document.getElementById('proof')).fontWeight === '700', undefined, { timeout: 10_000 })
    .then(() => true, () => false)
  const weight = await page.evaluate(() => getComputedStyle(document.getElementById('proof')).fontWeight)
  await page.route('**/virtual:stylex.css*', (route) => { counts.stylesheet410++; return route.fulfill({ status: 410, body: '' }) })
  await page.evaluate(async () => {
    for (let index = 0; index < 4; index++) await fetch(`/virtual:stylex.css?t=proof-${index}`, { cache: 'no-store' })
    await Promise.all(window.__hmrSockets.filter((socket) => socket.readyState === WebSocket.OPEN).map((socket) => new Promise((resolve) => {
      socket.addEventListener('close', resolve, { once: true })
      socket.close()
    })))
    console.error('synthetic error while the HMR socket is down')
    void Promise.reject(new Error('synthetic rejection while the HMR socket is down'))
  })
  await page.waitForTimeout(2_000)
  const receipt = { mode: withoutFix ? 'without-fix' : 'with-fix', vite: version, hmrUpdates: saves.length, finalFontWeight: weight, ...counts }
  console.log(JSON.stringify(receipt))
  if (!styled) throw new Error(`HMR did not apply the last StyleX save: font-weight ${weight}`)
  // The StyleX runtime may also refetch once after the last update; at least the four synthetic fetches must be refused.
  if (counts.stylesheet410 < 4) throw new Error('The synthetic stylesheet 410s did not reach the page')
  if (withoutFix) {
    if (counts.sendBeforeConnect < floodThreshold) throw new Error(`Negative control did not storm on Vite ${version}; select an affected Vite with VITE_MODULE`)
    console.log(`PASS negative control: with console forwarding Vite ${version} logged ${counts.sendBeforeConnect} send-before-connect errors`)
  } else {
    if (counts.sendBeforeConnect > 0) throw new Error(`${counts.sendBeforeConnect} send-before-connect errors after the HMR socket closed`)
    console.log('PASS StyleX HMR saves apply and a lost HMR socket logs no send-before-connect errors')
  }
} finally {
  await browser.close()
  await server.close()
  await rm(fixtureModule, { force: true })
  await rm(`${fixtureModule}.save`, { force: true })
  await rm(fixtureHtml, { force: true })
}
