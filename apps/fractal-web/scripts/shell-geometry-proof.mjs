#!/usr/bin/env node
/**
 * Browser proof: the real shell stays viewport-bound after a long roster populates, roster row
 * signals never overlap titles, and the header names no placeholder folder. Renders
 * src/web/shellGeometry.browser.tsx with the app's own Vite config (StyleX compiled, no gateway)
 * in managed Chromium at 1440x900; always closes its browser, server and scratch artifacts.
 *
 * `--without-fix` is the negative control: it reverts the row containing-block fix
 * (position:static on each row) and asserts the same proof FAILS with the document scrolling.
 * Both modes print one JSON receipt with the source revision, viewport and measured numbers.
 *
 *   node apps/fractal-web/scripts/shell-geometry-proof.mjs [--without-fix]
 */
import { execFileSync, spawn } from 'node:child_process'
import { randomBytes } from 'node:crypto'
import { mkdir, mkdtemp, rm } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { createServer, loadConfigFromFile } from 'vite'

const app = fileURLToPath(new URL('..', import.meta.url))
const withoutFix = process.argv.includes('--without-fix')
const git = (...args) => execFileSync('git', ['-C', app, ...args], { encoding: 'utf8' }).trim()
const source = { rev: git('rev-parse', 'HEAD'), dirty: git('status', '--porcelain', '--', '.').length > 0 }
const viewport = { width: 1440, height: 900 }
const scratchRoot = fileURLToPath(new URL('../../../tmp', import.meta.url))
await mkdir(scratchRoot, { recursive: true })
const scratch = await mkdtemp(`${scratchRoot}/shell-geometry-proof-`)
const session = `shell-geometry-${randomBytes(4).toString('hex')}`
const run = (args, capture = false) => {
  const { promise, resolve, reject } = Promise.withResolvers()
  const child = spawn('playwright-cli', [`-s=${session}`, ...args], { cwd: scratch, stdio: ['ignore', capture ? 'pipe' : 'inherit', 'inherit'] })
  let out = ''
  child.stdout?.on('data', (chunk) => { out += chunk })
  const timer = setTimeout(() => { child.kill('SIGTERM'); reject(new Error('Browser command deadline exceeded')) }, 60_000)
  child.on('error', (error) => { clearTimeout(timer); reject(error) })
  child.on('exit', (code) => { clearTimeout(timer); code === 0 ? resolve(out) : reject(new Error(`Browser command exited ${code}`)) })
  return promise
}
const loaded = await loadConfigFromFile({ command: 'serve', mode: 'development' }, `${app}/vite.config.ts`, app)
const server = await createServer({
  ...loaded.config,
  configFile: false,
  root: `${app}/src/web`,
  // The fixture owns its data; never connect to an inherited live gateway.
  plugins: loaded.config.plugins.flat().filter((plugin) => plugin?.name !== 'wf:shared-client-gateway'),
  server: { ...loaded.config.server, host: '127.0.0.1', port: 0, strictPort: false, hmr: false },
})
try {
  await server.listen()
  const { port } = server.httpServer.address()
  await run(['open', 'about:blank'])
  const output = await run(['run-code', `async (page) => {
    const errors = []
    page.on('console', (message) => { if (message.type() === 'error') errors.push(message.text() + ' ' + JSON.stringify(message.location())) })
    page.on('pageerror', (error) => errors.push(String(error)))
    await page.setViewportSize(${JSON.stringify(viewport)})
    ${withoutFix ? `await page.addInitScript(() => document.addEventListener('DOMContentLoaded', () => {
      const style = document.createElement('style')
      style.textContent = '[data-testid="taste-agent-row"] { position: static !important }'
      document.head.append(style)
    }))` : ''}
    await page.goto('http://127.0.0.1:${port}/shell-geometry.browser.html')
    await page.waitForFunction(() => document.body.dataset.testResult !== undefined, { timeout: 45000 })
    const proof = await page.evaluate(() => ({ result: document.body.dataset.testResult, proof: document.body.dataset.proof }))
    return 'PROOF ' + JSON.stringify({ ...proof, errors })
  }`], true)
  const line = output.split('\n').find((text) => text.includes('PROOF '))
  if (line === undefined) throw new Error(`No proof result in browser output: ${output}`)
  // playwright-cli prints the returned string JSON-quoted.
  const raw = line.slice(line.indexOf('PROOF '))
  const { result, proof, errors } = JSON.parse(JSON.parse(`"${raw.replace(/"\s*$/, '').slice('PROOF '.length)}"`))
  const facts = (() => { try { return JSON.parse(proof) } catch { return { detail: proof } } })()
  const populated = facts.populated
  const receipt = {
    mode: withoutFix ? 'without-fix' : 'with-fix', ...source, viewport, result,
    rows: populated?.rows, scrollHeight: populated?.scrollHeight, rosterScrollHeight: populated?.rosterScrollHeight,
    overlaps: facts.overlaps, failures: facts.failures ?? [facts.detail], browserErrors: errors.length,
  }
  console.log(JSON.stringify(receipt))
  if (withoutFix) {
    // The control must fail for the reason the fix addresses: the document grows past the viewport.
    if (result !== 'fail' || !(populated?.scrollHeight > viewport.height)) throw new Error('Negative control did not fail as expected')
    console.log(`PASS negative control: without the fix the document scrolls to ${populated.scrollHeight}px`)
  } else {
    if (result !== 'pass') throw new Error(proof)
    if (errors.length > 0) throw new Error('browser errors: ' + JSON.stringify(errors))
    console.log('PASS shell is viewport-bound after the roster populates; row signals clear titles; no placeholder folder')
  }
} finally {
  try { await run(['close']) } finally { await server.close(); await rm(scratch, { recursive: true, force: true }) }
}
