#!/usr/bin/env node
/** Live scratch-seat-only proof. Requires WF_ST_GATEWAY and WF_ST_AUTHORIZATION in the environment.
 * node apps/fractal-web/scripts/composer-send-proof.mjs [--without-binding]
 * The control removes only onNew's binding in Vite; it must issue no action and fail the send criterion.
 * HTTP and echo gates expose real Pending/Sent transitions; only the forced refusal is injected.
 */
import { execFileSync, spawn } from 'node:child_process'
import { randomBytes } from 'node:crypto'
import { readFile, mkdir, mkdtemp, rm } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { createServer, loadConfigFromFile } from 'vite'

const app = fileURLToPath(new URL('..', import.meta.url))
const seat = process.env.WF_E2E_SCRATCH_SEAT?.trim()
if (!seat) throw new Error('Set WF_E2E_SCRATCH_SEAT to a disposable scratch seat')
const rev = execFileSync('git', ['-C', app, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim()
const control = process.argv.includes('--without-binding')
const load = Number((await readFile('/proc/loadavg', 'utf8')).split(' ')[0])
const command = `node apps/fractal-web/scripts/composer-send-proof.mjs${control ? ' --without-binding' : ''}`
if (!Number.isFinite(load) || load >= 32) {
  console.log(JSON.stringify({ buildRev: rev, seat, load1: load, status: 'ready-not-run', command, timings: {} }))
  process.exit(0)
}
if (!process.env.WF_ST_GATEWAY || !process.env.WF_ST_AUTHORIZATION) throw new Error('Set WF_ST_GATEWAY and WF_ST_AUTHORIZATION for a send-granted scratch gateway; do not use a shared service')
const scratchRoot = fileURLToPath(new URL('../../../tmp', import.meta.url))
await mkdir(scratchRoot, { recursive: true })
const scratch = await mkdtemp(`${scratchRoot}/composer-send-proof-`)
const session = `composer-send-${randomBytes(4).toString('hex')}`
const run = (args, capture = false) => new Promise((resolve, reject) => {
  const child = spawn('playwright-cli', [`-s=${session}`, ...args], { cwd: scratch, stdio: ['ignore', capture ? 'pipe' : 'inherit', 'inherit'] })
  let output = ''
  child.stdout?.on('data', chunk => { output += chunk })
  const deadline = setTimeout(() => { child.kill('SIGTERM'); reject(new Error('Browser proof deadline exceeded')) }, 360_000)
  child.on('error', error => { clearTimeout(deadline); reject(error) })
  child.on('exit', code => { clearTimeout(deadline); code === 0 ? resolve(output) : reject(new Error(`Browser command exited ${code}: ${output}`)) })
})
const loaded = await loadConfigFromFile({ command: 'serve', mode: 'development' }, `${app}/vite.config.ts`, app)
let removedBinding = false
const server = await createServer({
  ...loaded.config, configFile: false, root: `${app}/src/web`,
  plugins: [...loaded.config.plugins.flat(), ...(control ? [{
    name: 'composer-send-negative-control', enforce: 'pre',
    transform(code, id) {
      if (!id.endsWith('/composerSend.ts')) return
      const changed = code.replace(/onNew: async \(message\) => \{[\s\S]*?\n      \},/, 'onNew: async () => {},')
      if (changed === code) throw new Error('Negative control could not remove the onNew binding')
      removedBinding = true
      return changed
    },
  }] : [])],
  server: { ...loaded.config.server, host: '127.0.0.1', port: 0, strictPort: false, hmr: false },
})
try {
  await server.listen()
  const port = server.httpServer.address().port
  if (port === 8445) throw new Error('Forbidden listener port')
  await run(['open', 'about:blank'])
  const result = await run(['run-code', `async page => {
    const seat = ${JSON.stringify(seat)}
    const origin = 'http://127.0.0.1:${port}'
    const timings = {}, started = Date.now(), actions = [], queuedEchoes = []
    const mark = name => { timings[name] = Date.now() - started }
    let holdEcho = true, releasePost, failNext = false, identity
    let postGate = new Promise(resolve => { releasePost = resolve })
    let firstArrival
    const firstAction = new Promise(resolve => { firstArrival = resolve })
    await page.routeWebSocket('**/v1/client/collections/stream*', socket => {
      const server = socket.connectToServer()
      server.onMessage(message => {
        const text = String(message)
        if (holdEcho && identity && text.includes(identity)) queuedEchoes.push(() => socket.send(message))
        else socket.send(message)
      })
    })
    await page.route('**/v1/client/actions', async route => {
      const action = route.request().postDataJSON()
      if (action.type !== 'message.send' || action.parameters.to !== seat) {
        await route.abort(); throw new Error('Refusing action outside the scratch message-send contract')
      }
      actions.push(action)
      firstArrival()
      identity = await page.evaluate(async key => {
        const bytes = new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(key)))
        return 'message/' + [...bytes.slice(0,8)].map(byte => byte.toString(16).padStart(2,'0')).join('')
      }, action.idempotency_key)
      if (failNext) {
        failNext = false
        await route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({
          api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', code: 'unavailable',
          message: 'Forced scratch-seat send failure', retryable: true, request_id: 'request/forced-proof', details: {},
        }) }); return
      }
      await postGate
      const response = await route.fetch()
      await route.fulfill({ response })
    })
    await page.goto(origin + '/w/' + encodeURIComponent(seat))
    const input = page.getByRole('textbox', { name: 'Message', exact: true })
    await input.waitFor({ state: 'visible', timeout: 60000 })
    if (await input.isDisabled()) throw new Error('Scratch-seat composer disabled: ' + await page.locator('body').innerText())
    const cancel = page.getByRole('button', { name: /^Cancel/ })
    if (await cancel.count() !== 0) throw new Error('Unavailable cancel control was rendered')
    const text = 'Scratch composer proof ' + Date.now()
    const row = page.locator('[data-testid="user-message"]').filter({ hasText: text })
    await input.fill(text); await input.press('Enter'); mark('submit')
    if (${control}) {
      let rejected = false
      try { await page.locator('[data-send-state="pending"]').filter({ hasText: text }).waitFor({ timeout: 2000 }) } catch { rejected = true }
      if (!rejected || actions.length !== 0) throw new Error('Negative control did not fail the submit criterion')
      mark('negativeControl')
      return 'RECEIPT ' + JSON.stringify({ status: 'negative-control-pass', timings, sends: actions.length })
    }
    await page.locator('[data-send-state="pending"]').filter({ hasText: text }).waitFor({ timeout: 60000 }); mark('pending')
    await firstAction
    if (actions.length !== 1) throw new Error('Submit did not issue exactly one Send')
    releasePost()
    await page.locator('[data-send-state="sent"]').filter({ hasText: text }).waitFor({ timeout: 60000 }); mark('sent')
    holdEcho = false; queuedEchoes.splice(0).forEach(release => release())
    await page.waitForFunction(text => [...document.querySelectorAll('[data-testid="user-message"]')].some(row => row.textContent.includes(text) && !row.dataset.itemId.startsWith('pending/')), text, { timeout: 60000 }); mark('echo')
    if (await row.count() !== 1) throw new Error('Echo duplicated the row')
    failNext = true; holdEcho = true
    const failureText = text + ' failure'
    await input.fill(failureText); await input.press('Enter')
    const failed = page.locator('[data-send-state="failed"]').filter({ hasText: failureText })
    await failed.waitFor({ timeout: 60000 }); mark('failed')
    await failed.getByRole('button', { name: 'failed', exact: true }).click()
    await failed.getByText('Forced scratch-seat send failure', { exact: true }).waitFor()
    const failedKey = actions[1].idempotency_key
    await failed.getByRole('button', { name: 'Retry', exact: true }).click()
    await page.locator('[data-send-state="sent"]').filter({ hasText: failureText }).waitFor({ timeout: 60000 }); mark('resend')
    if (actions.length !== 3 || actions[2].idempotency_key !== failedKey) throw new Error('Retry changed the key or issued extra actions')
    holdEcho = false; queuedEchoes.splice(0).forEach(release => release())
    await page.waitForFunction(text => [...document.querySelectorAll('[data-testid="user-message"]')].some(row => row.textContent.includes(text) && !row.dataset.itemId.startsWith('pending/')), failureText, { timeout: 60000 }); mark('resendEcho')
    return 'RECEIPT ' + JSON.stringify({ status: 'pass', timings, sends: actions.length, sameRetryKey: true })
  }`], true)
  const line = result.split('\n').find(line => line.includes('RECEIPT '))
  if (line === undefined) throw new Error('Missing browser receipt: ' + result)
  const encoded = line.trim()
  const value = encoded.startsWith('"') ? JSON.parse(encoded) : encoded
  const receipt = JSON.parse(value.slice(value.indexOf('RECEIPT ') + 'RECEIPT '.length))
  if (control && !removedBinding) throw new Error('Negative control never transformed the binding')
  console.log(JSON.stringify({ buildRev: rev, seat, load1: load, mode: control ? 'without-binding' : 'with-binding', ...receipt }))
} finally {
  try { await run(['close']) } finally { await server.close(); await rm(scratch, { recursive: true, force: true }) }
}
