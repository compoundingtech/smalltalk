#!/usr/bin/env node
/** Run the real React committed-paint fixture in managed Chromium; always close its browser/server. */
import { spawn } from 'node:child_process'
import { randomBytes } from 'node:crypto'
import { mkdir, mkdtemp, rm } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { createServer } from 'vite'

const app = fileURLToPath(new URL('..', import.meta.url))
const scratchRoot = fileURLToPath(new URL('../../../tmp', import.meta.url))
await mkdir(scratchRoot, { recursive: true })
const scratch = await mkdtemp(`${scratchRoot}/ux-paint-proof-`)
const session = `ux-paint-${randomBytes(4).toString('hex')}`
const run = (args) => new Promise((resolve, reject) => {
  const child = spawn('playwright-cli', [`-s=${session}`, ...args], { cwd: scratch, stdio: ['ignore', 'inherit', 'inherit'] })
  const timer = setTimeout(() => { child.kill('SIGTERM'); reject(new Error('Browser command deadline exceeded')) }, 30_000)
  child.on('error', (error) => { clearTimeout(timer); reject(error) })
  child.on('exit', (code) => { clearTimeout(timer); code === 0 ? resolve() : reject(new Error(`Browser command exited ${code}`)) })
})
const server = await createServer({ configFile: false, root: app, server: { host: '127.0.0.1', port: 0, fs: { allow: [fileURLToPath(new URL('../../..', import.meta.url))] } }, optimizeDeps: { include: ['react', 'react-dom/client', 'effect'] } })
try {
  await server.listen()
  const address = server.httpServer.address()
  await run(['open', `http://127.0.0.1:${address.port}/src/telemetry/ux.browser.html`])
  await run(['run-code', `async (page) => {
    await page.waitForFunction(() => document.body.dataset.testResult !== undefined, { timeout: 30000 })
    const proof = await page.evaluate(() => ({ result: document.body.dataset.testResult, proof: document.body.dataset.proof }))
    if (proof.result !== 'pass') throw new Error(JSON.stringify(proof))
    console.log('PASS real React transcript commit finishes wf.ux.switch only after paint', proof)
  }`])
} finally {
  try { await run(['close']) } finally { await server.close(); await rm(scratch, { recursive: true, force: true }) }
}
