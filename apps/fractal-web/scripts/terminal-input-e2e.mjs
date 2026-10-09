#!/usr/bin/env node
/** Live terminal input proof on a user-selected disposable scratch seat only.
 * node apps/fractal-web/scripts/terminal-input-e2e.mjs <baseUrl> <outDir> <seat>
 * Seat is required, with no default, and must contain the delimited word "scratch".
 * PLAYWRIGHT_MODULE: path or specifier of the playwright module (default 'playwright').
 * CHROMIUM_PATH: optional browser executable.
 * Pass = input enables, `echo <nonce>` echoes the nonce on its own grid row, Ctrl-C is accepted without
 * closing the input session, and the page never leaves the scratch seat.
 */
import fs from 'node:fs'
import { randomBytes } from 'node:crypto'

const [base, out, SEAT] = process.argv.slice(2)
if (!base || !out || !SEAT) {
  console.error('Usage: terminal-input-e2e.mjs <baseUrl> <outDir> <seat-containing-scratch>')
  process.exit(2)
}
// Typing reaches a real shell; require an explicitly named disposable scratch seat.
if (!/(?:^|[^a-z0-9])scratch(?:$|[^a-z0-9])/i.test(SEAT)) {
  console.error('Refusing input: the required seat must contain the word scratch')
  console.log('PASS false')
  process.exit(3)
}
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE ?? 'playwright')
fs.mkdirSync(out, { recursive: true })
const nonce = `e2e${randomBytes(6).toString('hex')}`
const url = `${base.replace(/\/$/, '')}/w/${SEAT}`
const browser = await chromium.launch(process.env.CHROMIUM_PATH ? { executablePath: process.env.CHROMIUM_PATH } : {})
const page = await (await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'dark' })).newPage()
const report = { url, nonce, steps: [], pass: false }
const step = (name, ok, extra = {}) => {
  report.steps.push({ name, ok, t: Date.now(), ...extra })
  console.log(name, ok ? 'ok' : 'FAIL', JSON.stringify(extra))
  if (!ok) throw new Error(`step ${name} failed`)
}
const onSeat = () => decodeURIComponent(new URL(page.url()).pathname) === `/w/${SEAT}`
const pane = page.locator('[data-testid="terminal-pane"]')
const grid = pane.locator('[aria-roledescription="terminal"]')
const inputState = () => pane.locator('[data-terminal-input-state]').getAttribute('data-terminal-input-state')
const echoed = () => grid.locator('[data-terminal-row]').evaluateAll(
  (rows, token) => rows.some((row) => row.textContent.trim() === token),
  nonce,
)
const poll = async (check, ms) => {
  for (const start = Date.now(); Date.now() - start < ms; await page.waitForTimeout(250)) if (await check()) return true
  return false
}
try {
  await page.goto(url)
  step('on-scratch-seat', onSeat(), { path: new URL(page.url()).pathname })
  const toggle = page.locator('[data-testid="thread-header"] button[aria-label="Toggle terminal drawer"]')
  await toggle.waitFor({ state: 'visible', timeout: 90_000 })
  if ((await toggle.getAttribute('aria-pressed')) !== 'true') await toggle.click()
  await grid.waitFor({ state: 'visible', timeout: 60_000 })
  step('grid-visible', true)
  const enable = pane.getByRole('button', { name: 'Enable input', exact: true })
  await enable.waitFor({ state: 'visible', timeout: 30_000 })
  await enable.click()
  step('input-ready', await poll(async () => (await inputState()) === 'Ready', 15_000))
  step('still-on-scratch-seat', onSeat())
  await page.keyboard.type(`echo ${nonce}`)
  await page.keyboard.press('Enter')
  step('nonce-echoed', await poll(echoed, 30_000))
  await page.keyboard.press('Control+C')
  await page.waitForTimeout(1500)
  step('ctrl-c-accepted', (await inputState()) === 'Ready' && (await pane.locator('[role="alert"]').count()) === 0)
  await page.screenshot({ path: `${out}/terminal-input.png` })
  await pane.getByRole('button', { name: 'Disable input', exact: true }).click()
  report.pass = report.steps.every((entry) => entry.ok)
} catch (error) {
  report.error = String(error).slice(0, 300)
  await page.screenshot({ path: `${out}/terminal-input-failure.png` }).catch(() => {})
}
fs.writeFileSync(`${out}/terminal-input-e2e.json`, JSON.stringify(report, null, 2))
console.log('PASS', report.pass)
await browser.close()
process.exit(report.pass ? 0 : 1)
