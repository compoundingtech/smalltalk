#!/usr/bin/env node
/**
 * Live proof of the conversation alert strip against a real, isolated st.
 *
 *   node apps/fractal-web/scripts/alerts-live-proof.mjs [run|up|down]
 *
 * `up` makes a throwaway st: the instance `webproof` under its own HOME in WEBPROOF_ROOT
 * (default /var/tmp/st-webproof), so neither `~/.st-instance` nor the default install, its
 * daemon or its service units are touched. It founds a one-machine dial-out fleet, starts the
 * daemon in a transient user unit (`webproof-st-up`), declares person/avery and one program
 * seat, agent/example/atlas, and pairs a full-control device for avery whose bearer credential
 * stays in WEBPROOF_ROOT/authorization (mode 0600, never printed).
 * `run` (the default) brings it up when it is not, has atlas ask avery a yes-or-no question and
 * a structured decision, serves the app on loopback with WF_ST_GATEWAY/WF_ST_AUTHORIZATION set
 * to the instance's paired gateway, opens atlas's conversation in headless Chromium, answers
 * both from the strip (Yes, then the recommended decision; each confirmed with Send) and checks
 * with st that each step finished with that answer. It prints one JSON receipt: `firstDataMs`
 * (navigation to the ask in the strip, warm; `coldFirstDataMs` includes Vite's first optimize),
 * and per answer `answerToClearedMs` (Send click to the first frame painted without the alert,
 * against the 2000 ms alert_landed budget), the client requests the answer made, and
 * `alertFrameMs`, when st's collection stream first pushed a frame naming the alert. Earlier
 * runs' unanswered asks are cleared first. Screenshots go to OUT (default WEBPROOF_ROOT/out).
 * `down` stops the daemon, uninstalls the instance and removes WEBPROOF_ROOT.
 *
 * Each st command runs in its own transient user unit (`systemd-run --user`) with a scrubbed
 * environment: the daemon attributes a local caller by the ST_AGENT of its process ancestors, so
 * from inside an st seat a plain call would speak for that seat and its daemon. Nothing here
 * reaches the default daemon or any real person.
 *
 * Needs: systemd user manager; ST3_BINARY and PTY_BINARY (default: `st3` and `pty` on PATH);
 * a Playwright module and Chromium, for example from the repository's nixpkgs:
 *   nix build --inputs-from . nixpkgs#playwright-driver nixpkgs#playwright-driver.browsers-chromium --no-link --print-out-paths
 *   PLAYWRIGHT_MODULE=<playwright-core>/index.mjs PLAYWRIGHT_BROWSERS_PATH=<playwright-browsers> \
 *     nix develop .#web -c node apps/fractal-web/scripts/alerts-live-proof.mjs
 */
import { execFileSync, spawnSync } from 'node:child_process'
import { randomBytes } from 'node:crypto'
import { chmodSync, copyFileSync, existsSync, mkdirSync, readFileSync, rmSync, symlinkSync, writeFileSync } from 'node:fs'
import { request } from 'node:http'
import { basename, dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const app = fileURLToPath(new URL('..', import.meta.url))
const root = process.env.WEBPROOF_ROOT ?? '/var/tmp/st-webproof'
const out = process.env.OUT ?? join(root, 'out')
const instance = 'webproof'
const person = 'person/avery'
const seat = 'agent/example/atlas'
const unit = 'webproof-st-up'
const home = join(root, 'home')
const tools = join(root, 'tools')
const runDir = join(home, '.st-instance', instance, 'run')
const gateway = join(runDir, 'st3-client.sock')
const authorizationFile = join(root, 'authorization')
const BUDGET_MS = 2000

const which = (name) => execFileSync('sh', ['-c', 'command -v "$1"', 'sh', name], { encoding: 'utf8' }).trim()
const env = which('env')
const sleep = which('sleep')
const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms))

/** A transient user unit running ARGV with the instance's clean environment. */
const isolated = (argv) => [env, '-i', `HOME=${home}`, `PATH=${tools}:${dirname(sleep)}:/usr/bin:/bin`, `USER=${process.env.USER ?? 'webproof'}`, 'LANG=C.UTF-8', 'SHELL=/bin/sh', ...argv]
const st = (args, { check = true } = {}) => {
  const result = spawnSync('systemd-run', ['--user', '--wait', '--pipe', '--collect', '--quiet', ...isolated([join(tools, 'st3'), '--instance', instance, ...args])], { encoding: 'utf8' })
  if (check && result.status !== 0) throw new Error(`st ${args.join(' ')} exited ${result.status}: ${result.stderr || result.stdout}`)
  return result
}
const stJson = (args) => JSON.parse(st(['--json', ...args]).stdout)
const alive = () => existsSync(gateway) && st(['--daemon-wait', '0', 'agents', 'ls'], { check: false }).status === 0

/** One unauthenticated request to the paired gateway socket (pairing completion). */
const gatewayPost = (path, body) => new Promise((resolve, reject) => {
  const payload = JSON.stringify(body)
  const req = request({ socketPath: gateway, path, method: 'POST', headers: { host: '127.0.0.1', 'content-type': 'application/json', 'content-length': Buffer.byteLength(payload) } }, (res) => {
    let text = ''
    res.on('data', (chunk) => { text += chunk })
    res.on('end', () => res.statusCode === 200 ? resolve(JSON.parse(text)) : reject(new Error(`POST ${path}: ${res.statusCode} ${text}`)))
  })
  req.on('error', reject)
  req.end(payload)
})

async function up() {
  if (!alive()) await start()
  // A program seat: it needs no harness sign-in, only to exist in the roster and run.
  const seats = join(root, 'seats.kdl')
  writeFileSync(seats, `version 2\nagent "${seat.slice('agent/'.length)}" {\n  workspace "${join(root, 'workspace')}"\n  argv "${sleep}" "infinity"\n}\n`)
  st(['apply', seats, '--as', person])
  for (let i = 0; i < 60 && !/running/.test(st(['agents', 'ls']).stdout); i++) await pause(500)
  if (!existsSync(authorizationFile)) {
    // Pair as a web device would: begin locally for avery, complete on the paired gateway.
    const challenge = stJson(['devices', 'pair', 'Webproof browser', '--full-control', '--as', person]).value
    // A non-signing device key: the strip's answers need no message signing.
    const session = await gatewayPost(`/v1/client/pairings/${encodeURIComponent(challenge.pairing_id.replace(/^pairing\//, ''))}/complete`, {
      api_version: 'st3.client.v0', code: challenge.code, device_public_key: `webproof-${randomBytes(24).toString('base64url')}`,
    })
    const credential = session.value?.credential ?? session.credential
    if (!credential) throw new Error('Pairing returned no credential')
    writeFileSync(authorizationFile, `Bearer ${credential}`, { mode: 0o600 })
  }
}

async function start() {
  mkdirSync(tools, { recursive: true })
  mkdirSync(join(root, 'workspace'), { recursive: true })
  if (!existsSync(join(tools, 'st3'))) {
    copyFileSync(process.env.ST3_BINARY ?? which('st3'), join(tools, 'st3'))
    copyFileSync(process.env.PTY_BINARY ?? which('pty'), join(tools, 'pty')) // st3 finds pty beside itself
    chmodSync(join(tools, 'st3'), 0o755); chmodSync(join(tools, 'pty'), 0o755)
    symlinkSync('st3', join(tools, 'st'))
  }
  // The daemon checks that `pty` is on the login shell's PATH.
  mkdirSync(home, { recursive: true })
  for (const profile of ['.profile', '.bash_profile', '.zprofile']) writeFileSync(join(home, profile), `export PATH="${tools}:$PATH"\n`)
  if (!existsSync(join(root, 'fleet-created'))) {
    st(['fleet', 'create', '--no-service', '--dial-out', '--name', instance])
    writeFileSync(join(root, 'fleet-created'), '')
  }
  spawnSync('systemctl', ['--user', 'reset-failed', unit], { stdio: 'ignore' })
  execFileSync('systemd-run', ['--user', `--unit=${unit}`, '--collect', '--quiet', ...isolated([join(tools, 'st3'), '--instance', instance, 'up'])])
  for (let i = 0; i < 120 && !alive(); i++) await pause(500)
  if (!alive()) throw new Error(`The instance daemon did not start; see journalctl --user -u ${unit}`)
  if (!existsSync(join(root, 'setup-done'))) {
    st(['setup', '--person', person.slice('person/'.length), '--yes', '--service', 'false', '--install', 'false', '--harness', 'none', '--claude-channel', 'false', '--expert', 'false'])
    writeFileSync(join(root, 'setup-done'), '')
  }
}

function down() {
  // WEBPROOF_ROOT comes from the environment: act only on a directory this script made,
  // so a wrong root neither uninstalls nor deletes anything.
  if (!basename(root).startsWith('st-webproof') || !existsSync(join(root, 'fleet-created'))) {
    console.error(`not removing ${root}: it is not named st-webproof* or has no fleet-created marker from this script`)
    process.exitCode = 1
    return
  }
  spawnSync('systemctl', ['--user', 'stop', unit], { stdio: 'inherit' })
  if (existsSync(tools)) {
    const result = st(['uninstall', '--yes', '--erase-local-graph', '--no-service', '--offline'], { check: false })
    process.stdout.write(result.stdout + result.stderr)
  }
  rmSync(root, { recursive: true, force: true })
  console.log(`removed ${root}`)
}

/** The step behind an alert, as st reports it once answered. */
const stepAnswer = (step) => {
  const view = stJson(['work', 'show', step]).value
  const last = view.person_answers?.at(-1)
  return { state: view.state, summary: last?.summary ?? null, answer: last?.answer?.id ?? null }
}

/** Earlier runs' unanswered asks from the seat, so the strip shows only this run's. */
const clearStale = () => {
  for (const item of stJson(['alerts', 'ls', '--as', person]).value.items) {
    if (item.state !== 'open' || item.requester_id !== seat || !item.actions.includes('work.done')) continue
    st(['work', 'done', item.source_id, '--as', person, ...(item.request?.answers?.length ? ['--answer', 'dismiss'] : ['--summary', 'Cleared before a proof run.'])])
  }
}

async function proof() {
  await up()
  clearStale()
  const nonce = randomBytes(3).toString('hex')
  const ask = (title, args) => {
    const result = stJson(['work', 'ask', '--for', person, '--new-run', `webproof-${nonce}-${title.length}`, '--title', title, '--as', seat, '--idempotency-key', `webproof-${nonce}-${title}`, ...args])
    return result.subject
  }
  const decisionTitle = `Publish example notes ${nonce}?`
  const request = join(root, 'decision.json')
  writeFileSync(request, JSON.stringify({
    version: 1, type: 'decision', question: decisionTitle, why_person: 'The owner decides what the example publishes.',
    reasons: ['The notes are drafted.'], recommendation: { answer: 'publish', reason: 'They are ready.' },
    answers: [
      { id: 'publish', label: 'Publish the notes', outcome: 'accept', consequence: 'I publish them.' },
      { id: 'hold', label: 'Hold them', outcome: 'decline', consequence: 'Nothing is published.' },
      { id: 'revise', label: 'Request changes', outcome: 'request_changes', consequence: 'I revise and ask again.' },
    ],
  }))
  const decisionStep = ask(decisionTitle, ['--request', request])
  // The newest alert shows open, so the yes-or-no ask comes last.
  const yesTitle = `Ship example build ${nonce}?`
  const yesStep = ask(yesTitle, ['--reason', 'Answer yes or no.'])
  const alertIds = new Map(stJson(['alerts', 'ls', '--as', person]).value.items.map((item) => [item.title, item.id]))

  process.env.WF_ST_GATEWAY = gateway
  process.env.WF_ST_AUTHORIZATION = readFileSync(authorizationFile, 'utf8').trim()
  const { createServer, loadConfigFromFile } = await import('vite')
  const loaded = await loadConfigFromFile({ command: 'serve', mode: 'development' }, `${app}/vite.config.ts`, app)
  // The gateway's upgrade handler answers every non-client upgrade with 403 on the same listener,
  // which corrupts Vite's HMR socket and makes its client reload the page; so HMR's socket is off.
  const server = await createServer({ ...loaded.config, configFile: false, root: `${app}/src/web`, server: { ...loaded.config.server, host: '127.0.0.1', port: 0, strictPort: false, hmr: false, ws: false } })
  const { chromium } = await import(process.env.PLAYWRIGHT_MODULE ?? 'playwright')
  // Nix's browsers ship full Chromium, not the headless shell.
  const browser = await chromium.launch(process.env.CHROMIUM_PATH ? { executablePath: process.env.CHROMIUM_PATH } : { channel: 'chromium' })
  mkdirSync(out, { recursive: true })
  const errors = []
  const page = await (await browser.newContext({ viewport: { width: 1280, height: 900 } })).newPage()
  try {
    await server.listen()
    const origin = `http://127.0.0.1:${server.httpServer.address().port}`
    page.on('console', (message) => { if (message.type() === 'error') errors.push(message.text()) })
    page.on('pageerror', (error) => errors.push(String(error)))
    // Collection frames as they arrive, to tell st's push from the app's paint.
    const frames = []
    page.on('websocket', (socket) => socket.on('framereceived', ({ payload }) => frames.push({ at: Date.now(), payload: String(payload) })))
    const strip = page.locator('[data-testid="conversation-alerts"]')
    // The first load waits on Vite's dependency optimizer; the reload measures a warm page.
    const firstData = async (load) => {
      await load()
      await strip.locator(`section[aria-label="${yesTitle}"]`).waitFor({ state: 'attached', timeout: 120_000 })
      return Math.round(await page.evaluate(() => performance.now()))
    }
    const coldFirstDataMs = await firstData(() => page.goto(`${origin}/w/${seat}?open=thread`, { waitUntil: 'commit', timeout: 120_000 }))
    const firstDataMs = await firstData(() => page.reload({ waitUntil: 'commit' }))
    await strip.locator(`section[aria-label="${decisionTitle}"]`).waitFor({ state: 'attached', timeout: 60_000 })
    // Without HMR, StyleX's dev stylesheet misses lazily loaded modules until fetched again.
    const shoot = async (name) => {
      await page.evaluate(async () => {
        const style = document.getElementById('__stylex_virtual__')
        if (style) style.textContent = await (await fetch(`/virtual:stylex.css?t=${Date.now()}`, { cache: 'no-store' })).text()
      })
      await page.screenshot({ path: join(out, name) })
    }
    await shoot('alerts-before.png')

    /** Click CHOICE then Send in the alert's confirm group; time Send to the paint without it. */
    const answer = async (title, choice, shot) => {
      const section = strip.locator(`section[aria-label="${title}"]`)
      await section.evaluate((node) => { if (!node.checkVisibility()) node.closest('details')?.setAttribute('open', '') })
      await section.getByRole('button', { name: choice, exact: true }).click()
      const send = section.getByRole('group', { name: 'Confirm your answer' }).getByRole('button', { name: 'Send', exact: true })
      await send.waitFor()
      // The in-page clock starts on the Send click and stops at the first frame painted without the alert.
      await page.evaluate((title) => {
        let sentAt
        document.addEventListener('click', () => { sentAt ??= performance.now() }, { capture: true, once: true })
        const gone = () => ![...document.querySelectorAll('[data-testid="conversation-alerts"] section')].some((node) => node.getAttribute('aria-label') === title)
        window.__alertCleared = new Promise((resolve) => {
          // Where the time went: each client request the answer made, as ms after the click.
          const requests = () => performance.getEntriesByType('resource')
            .filter((entry) => entry.startTime >= sentAt && entry.name.includes('/v1/client/'))
            .map((entry) => `${new URL(entry.name).pathname} ${Math.round(entry.startTime - sentAt)}-${Math.round(entry.responseEnd - sentAt)}`)
          const check = () => sentAt !== undefined && gone()
            ? requestAnimationFrame(() => resolve({ ms: Math.round(performance.now() - sentAt), requests: requests() }))
            : setTimeout(check, 5)
          check()
        })
      }, title)
      const clickedAt = Date.now()
      await send.click()
      const cleared = await Promise.race([page.evaluate(() => window.__alertCleared), pause(30_000).then(() => null)])
      const ms = cleared?.ms ?? null
      // A refusal stays in the alert, in the words the person reads.
      const status = cleared === null ? await section.getByRole('status').textContent().catch(() => null) : undefined
      await shoot(shot)
      // The first collection frame after the click that names the alert: st's push of the change.
      const pushed = frames.find((frame) => frame.at >= clickedAt && frame.payload.includes(alertIds.get(title)))
      return {
        answerToClearedMs: ms, budgetMet: ms !== null && ms < BUDGET_MS, requests: cleared?.requests,
        alertFrameMs: pushed === undefined ? null : pushed.at - clickedAt, ...(status === undefined ? {} : { status }),
      }
    }
    const yes = await answer(yesTitle, 'Yes', 'alerts-after-yes.png')
    const decision = await answer(decisionTitle, 'Publish the notes (recommended)', 'alerts-after-decision.png')
    const yesSt = stepAnswer(yesStep)
    const decisionSt = stepAnswer(decisionStep)
    const receipt = {
      status: yes.answerToClearedMs !== null && yesSt.state === 'completed' && yesSt.summary === 'Yes' &&
        decision.answerToClearedMs !== null && decisionSt.state === 'completed' && decisionSt.answer === 'publish' ? 'pass' : 'fail',
      buildRev: execFileSync('git', ['-C', app, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(),
      // Timings move with host load; compare runs on a quiet host.
      load1: Number(readFileSync('/proc/loadavg', 'utf8').split(' ')[0]),
      budgetMs: BUDGET_MS,
      coldFirstDataMs,
      firstDataMs,
      yes: { ...yes, step: yesStep, st: yesSt },
      decision: { ...decision, step: decisionStep, st: decisionSt },
      openAlerts: stJson(['alerts', 'ls', '--as', person]).value.items.filter((item) => item.state === 'open').map((item) => item.title),
      screenshots: ['alerts-before.png', 'alerts-after-yes.png', 'alerts-after-decision.png'].map((name) => join(out, name)),
      // Vite's own HMR socket is off (see the server options), so its client's failures are expected.
      consoleErrors: errors.filter((text) => !text.includes('/?token=')),
    }
    console.log(JSON.stringify(receipt, null, 2))
    if (receipt.status !== 'pass') process.exitCode = 1
  } catch (error) {
    await page.screenshot({ path: join(out, 'alerts-failure.png') }).catch(() => {})
    console.error(JSON.stringify({ status: 'error', screenshot: join(out, 'alerts-failure.png'), consoleErrors: errors }, null, 2))
    throw error
  } finally {
    await browser.close()
    await server.close()
  }
}

const command = process.argv[2] ?? 'run'
if (command === 'up') { await up(); console.log(`up: WF_ST_GATEWAY=${gateway} WF_ST_AUTHORIZATION=$(cat ${authorizationFile})`) }
else if (command === 'down') down()
else if (command === 'run') await proof()
else throw new Error('usage: alerts-live-proof.mjs [run|up|down]')
