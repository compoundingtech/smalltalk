#!/usr/bin/env node
/** Live read-only terminal failure proof for one named seat.
 * node apps/fractal-web/scripts/terminal-failure-e2e.mjs <baseUrl> <outDir> <seat>
 * Seat is `agent/<host>/<name>` or `<host>/<name>`. PLAYWRIGHT_MODULE and CHROMIUM_PATH as in
 * terminal-input-e2e.mjs.
 * The probe never types, fills, enables input or sends: every action except `terminal.attach` is
 * aborted and counted as a violation. It only clicks the header terminal toggle.
 * Pass = the thread transcript, composer, header Resources control and breadcrumb host stay in place
 * (the same DOM nodes across the toggle and an in-app route change) while the terminal surface shows
 * either a live grid or a specific unavailable reason: in the drawer when the toggle is enabled, in
 * the disabled toggle's description otherwise. A disabled toggle also gets an in-app route change to
 * `?open=terminal/<host>/<name>:detail`, which must show that reason in a drawer beside the same
 * thread nodes; both toggles then load that route cold. The receipt records runtime reads, terminal
 * attaches and the selected roster row's `runtime_ids` as observed over HTTP and the collections socket.
 * NO_TERMINAL=1 requires the named toggle to be disabled and explained from its first visible render,
 * before waiting for the transcript or roster; this catches an enabled control during roster loading.
 */
import fs from 'node:fs'

const [base, out, SEAT] = process.argv.slice(2)
if (!base || !out || !SEAT) {
  console.error('Usage: terminal-failure-e2e.mjs <baseUrl> <outDir> <seat>')
  process.exit(2)
}
const agentRef = SEAT.startsWith('agent/') ? SEAT : `agent/${SEAT}`
const seatPath = agentRef.slice('agent/'.length)
const origin = base.replace(/\/$/, '')
const threadPath = `/w/${agentRef.split('/').map(encodeURIComponent).join('/')}`
const terminalPane = `terminal/${seatPath}:detail`
const terminalUrl = `${threadPath}?open=${encodeURIComponent(terminalPane)}`
// Copy that names no cause: a drawer showing only these has not explained the failure.
const GENERIC = [
  'The terminal could not be opened. Return to the thread and open it again.',
  'Loading terminal…',
  'Terminal unavailable',
]

const { chromium } = await import(process.env.PLAYWRIGHT_MODULE ?? 'playwright')
fs.mkdirSync(out, { recursive: true })
const browser = await chromium.launch(process.env.CHROMIUM_PATH ? { executablePath: process.env.CHROMIUM_PATH } : {})
const page = await (await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'dark' })).newPage()
const report = {
  url: origin + threadPath,
  agentRef,
  steps: [],
  network: { runtimeGets: [], terminalAttaches: 0, abortedActions: [], terminalStreams: 0 },
  roster: { framesSeen: 0, frames: [], agentIdsSeen: 0, selected: [] },
  pass: false,
}
// Surface assertions record and continue, so one receipt shows every violated expectation.
const check = (name, ok, extra = {}) => {
  report.steps.push({ name, ok, t: Date.now(), ...extra })
  console.log(name, ok ? 'ok' : 'FAIL', JSON.stringify(extra))
}
// Preconditions stop the run: later assertions would only measure the wrong page.
const step = (name, ok, extra = {}) => {
  check(name, ok, extra)
  if (!ok) throw new Error(`step ${name} failed`)
}
const path = (url) => url.replace(/^https?:\/\/[^/]+/, '').replace(/\?.*$/, '')

// Roster evidence: only the selected row's runtime/host fields, plus a count of rows seen.
const agentIds = new Set()
const recordRoster = (via, value, collection) => {
  report.roster.framesSeen += 1
  // Coverage only: a page that is not the whole roster cannot prove the seat is absent.
  const items = Array.isArray(value?.items) ? value.items : Array.isArray(value?.value?.items) ? value.value.items : undefined
  report.roster.frames.push({
    via, collection, at: Date.now(),
    items: items?.length ?? null,
    has_more: value?.has_more ?? value?.value?.has_more ?? null,
    cursor: (value?.next_cursor ?? value?.value?.next_cursor) == null ? null : 'present',
    containsSelected: JSON.stringify(value).includes(`"${agentRef}"`),
  })
  const visit = (node, depth) => {
    if (node === null || typeof node !== 'object' || depth > 8) return
    if (Array.isArray(node)) { for (const item of node) visit(item, depth + 1); return }
    if (typeof node.id === 'string' && node.id.startsWith('agent/')) agentIds.add(node.id)
    if (node.id === agentRef) {
      report.roster.selected.push({
        via, collection, at: Date.now(),
        runtime_ids: node.runtime_ids ?? '(field absent)',
        host: node.host_id ?? node.host ?? '(field absent)',
        fields: Object.keys(node).sort(),
      })
    }
    for (const value of Object.values(node)) visit(value, depth + 1)
  }
  visit(value, 0)
  report.roster.agentIdsSeen = agentIds.size
}
const parse = (text) => { try { return JSON.parse(text) } catch { return undefined } }

page.on('request', (request) => {
  const url = request.url()
  if (request.method() === 'GET' && /\/v1\/client\/runtimes\//.test(url))
    report.network.runtimeGets.push({ path: path(url), at: Date.now() })
})
page.on('response', async (response) => {
  if (response.request().method() !== 'GET' || !/\/v1\/client\/agents(?:$|[/?])/.test(response.url())) return
  const body = await response.json().catch(() => undefined)
  if (body !== undefined) recordRoster(`http ${response.status()} ${path(response.url())}`, body, 'agents')
})
page.on('websocket', (socket) => {
  if (/\/v1\/client\/terminals\//.test(socket.url())) report.network.terminalStreams += 1
  if (!/\/v1\/client\/collections\/stream/.test(socket.url())) return
  socket.on('framereceived', (frame) => {
    const message = parse(String(frame.payload))
    if (message?.collection === 'agents' || (message !== undefined && JSON.stringify(message).includes(`"${agentRef}"`)))
      recordRoster(`ws ${message.kind ?? 'frame'}`, message, message.collection)
  })
})
// Read-only guard: viewing a terminal needs only `terminal.attach`; anything else is refused here.
await page.route('**/v1/client/actions', async (route) => {
  const action = parse(route.request().postData() ?? '')
  if (route.request().method() === 'POST' && action?.type === 'terminal.attach') {
    report.network.terminalAttaches += 1
    await route.continue()
    return
  }
  report.network.abortedActions.push(action?.type ?? route.request().method())
  await route.abort()
})

const header = page.locator('[data-testid="thread-header"]')
// Match the live probe's page-level accessible control, not a wrapper or a second app-only toggle.
const toggle = page.getByRole('button', { name: 'Toggle terminal drawer', exact: true })
const disabledWrap = header.locator('[data-testid="terminal-toggle-disabled"]')
const drawer = page.locator('[data-testid="terminal-pane"]')
const grid = drawer.locator('[aria-roledescription="terminal"]')
const composer = page.getByRole('textbox', { name: 'Message', exact: true })

/** The settled transcript (not the lazy fallback) and composer, as live element handles. */
const settledThread = async () => {
  await page.waitForFunction(() => {
    const lane = document.querySelector('[aria-label="Transcript"]')
    return lane !== null && lane.querySelector('[data-testid="transcript-placeholder"]') === null
      && lane.querySelector('[data-testid="transcript-turn"], [data-testid="transcript-empty"], [data-testid="transcript-unavailable"]') !== null
  }, undefined, { timeout: 90_000 })
  await composer.waitFor({ state: 'visible', timeout: 30_000 })
}
/** Pins the current transcript, composer, Resources and breadcrumb nodes for identity checks. */
const pin = () => page.evaluate(() => {
  const head = document.querySelector('[data-testid="thread-header"]')
  window.__wfTerminalFailureProbe = {
    transcript: document.querySelector('[aria-label="Transcript"]'),
    composer: document.querySelector('textarea[aria-label="Message"]') ?? document.querySelector('textarea'),
    breadcrumb: head?.querySelector('nav[aria-label="Breadcrumb"]') ?? null,
  }
})
const surroundings = () => page.evaluate(() => {
  const visible = (node) => node instanceof HTMLElement && node.isConnected && node.getClientRects().length > 0
    && getComputedStyle(node).visibility !== 'hidden' && node.closest('[hidden], [inert]') === null
  const pinned = window.__wfTerminalFailureProbe ?? {}
  const head = document.querySelector('[data-testid="thread-header"]')
  const resources = [...(head?.querySelectorAll('button') ?? [])].filter((button) => button.getAttribute('aria-label') === 'Resources')
  const crumb = head?.querySelector('nav[aria-label="Breadcrumb"]')
  // The kit renders `<folder><slash aria-hidden>/</slash><title>`; only a present folder is a host.
  const slash = crumb?.querySelector(':scope > [aria-hidden="true"]')
  const transcript = document.querySelector('[aria-label="Transcript"]')
  const composer = document.querySelector('textarea[aria-label="Message"]') ?? document.querySelector('textarea')
  return {
    transcriptVisible: visible(transcript),
    transcriptSameNode: transcript != null && pinned.transcript === transcript,
    composerVisible: visible(composer),
    composerSameNode: composer != null && pinned.composer === composer,
    composerValueLength: composer?.value.length ?? -1,
    resourcesCount: resources.length,
    resourcesVisible: resources.length === 1 && visible(resources[0]) && resources[0].textContent.trim() === 'Resources',
    breadcrumb: crumb?.textContent.trim() ?? '',
    breadcrumbHost: slash?.previousElementSibling?.textContent.trim() ?? '',
    breadcrumbSameNode: crumb != null && pinned.breadcrumb === crumb,
    drawers: document.querySelectorAll('[data-testid="terminal-pane"]').length,
  }
})
const surroundingsHold = (state, before, identity) =>
  state.transcriptVisible && state.composerVisible && state.resourcesVisible && state.breadcrumb === before.breadcrumb
  && state.breadcrumbHost === before.breadcrumbHost && state.composerValueLength === before.composerValueLength
  && (!identity || (state.transcriptSameNode && state.composerSameNode && state.breadcrumbSameNode))
/** Waits for the drawer to settle on a live grid or a non-generic reason; returns what it shows. */
const drawerOutcome = async () => {
  await drawer.waitFor({ state: 'visible', timeout: 30_000 })
  for (const start = Date.now(); Date.now() - start < 45_000; await page.waitForTimeout(250)) {
    if (await grid.isVisible().catch(() => false)) return { kind: 'grid' }
    const text = (await drawer.innerText().catch(() => '')).trim()
    if (text !== '' && text !== 'Loading terminal…') {
      // Hold briefly so a transient reason that the attach immediately replaces is not counted.
      await page.waitForTimeout(1500)
      if (await grid.isVisible().catch(() => false)) return { kind: 'grid' }
      const settled = (await drawer.innerText().catch(() => '')).trim()
      if (settled === text) {
        const retry = await drawer.getByRole('button', { name: 'Retry', exact: true }).count()
        const state = await drawer.locator('[data-terminal-state]').first().getAttribute('data-terminal-state').catch(() => null)
        return { kind: 'reason', text, retry, state, label: await drawer.getAttribute('aria-label') }
      }
    }
  }
  return { kind: 'timeout', text: (await drawer.innerText().catch(() => '')).trim() }
}
const specific = (text) => text !== '' && !GENERIC.some((generic) => text === generic || text.startsWith(`${generic}\n`))
const shot = (name) => page.screenshot({ path: `${out}/${name}.png` }).catch(() => {})
const toggleState = () => toggle.evaluate((button) => ({
  disabled: button.disabled || button.getAttribute('aria-disabled') === 'true',
  title: button.title || '',
  describedBy: (button.getAttribute('aria-describedby') ?? '').split(/\s+/).filter(Boolean)
    .map((id) => document.getElementById(id)?.textContent?.trim()).filter(Boolean).join(' | '),
}))

try {
  await page.goto(origin + threadPath)
  step('on-seat', decodeURIComponent(new URL(page.url()).pathname) === `/w/${agentRef}`, { path: new URL(page.url()).pathname })
  await header.waitFor({ state: 'visible', timeout: 90_000 })
  await toggle.waitFor({ state: 'visible', timeout: 60_000 })
  step('single-header-toggle', await toggle.count() === 1 && await header.getByRole('button', { name: 'Toggle terminal drawer', exact: true }).count() === 1)
  // Capture the same button attributes at the same early point as the live baseline probe.
  report.firstToggle = await toggleState()
  if (process.env.NO_TERMINAL) check('no-terminal-first-control', report.firstToggle.disabled
    && specific(report.firstToggle.describedBy || report.firstToggle.title)
    && /terminal/i.test(report.firstToggle.describedBy || report.firstToggle.title), report.firstToggle)
  await settledThread()
  await page.waitForFunction(() => {
    const crumb = document.querySelector('[data-testid="thread-header"] nav[aria-label="Breadcrumb"]')
    return crumb?.querySelector(':scope > [aria-hidden="true"]') != null
  }, undefined, { timeout: 10_000 }).catch(() => {})
  await pin()
  const before = await surroundings()
  // An agent outside the observed roster has no host to show; record it rather than invent one.
  report.baselineHost = before.breadcrumbHost === '' ? null : before.breadcrumbHost
  step('thread-baseline', surroundingsHold(before, before, true) && before.drawers === 0, before)
  await shot('baseline')

  const settledToggle = await toggleState()
  report.settledToggle = settledToggle
  const disabled = settledToggle.disabled
  const enabled = !disabled && await toggle.isEnabled()
  report.toggle = disabled ? 'disabled' : enabled ? 'enabled' : 'absent'
  step('toggle-present', disabled || enabled, { toggle: report.toggle })
  if (process.env.NO_TERMINAL) check('no-terminal-settled-control', disabled
    && specific(settledToggle.describedBy || settledToggle.title)
    && /terminal/i.test(settledToggle.describedBy || settledToggle.title), settledToggle)

  if (enabled) {
    if ((await toggle.getAttribute('aria-pressed')) === 'true') step('drawer-initially-closed', false)
    await toggle.click()
    const outcome = await drawerOutcome()
    report.drawer = outcome
    await shot('drawer-open')
    const after = await surroundings()
    check('thread-retained-with-drawer', surroundingsHold(after, before, true) && after.drawers === 1, after)
    check('drawer-shows-terminal-or-specific-reason', outcome.kind === 'grid' || (outcome.kind === 'reason' && specific(outcome.text)), outcome)
    await toggle.click()
    await drawer.waitFor({ state: 'detached', timeout: 15_000 })
    const closed = await surroundings()
    check('thread-retained-after-close', surroundingsHold(closed, before, true) && closed.drawers === 0, closed)
  } else {
    const reason = (await disabledWrap.getAttribute('title'))?.trim() ?? ''
    const description = settledToggle.describedBy
    report.disabledReason = reason
    check('disabled-toggle-reason', specific(reason) && description === reason
      && await toggle.isDisabled(), { reason, description, control: settledToggle })

    // In-app route change: the shell keeps the thread nodes while the drawer opens beside them.
    await page.evaluate((url) => {
      window.history.pushState(null, '', url)
      window.dispatchEvent(new PopStateEvent('popstate'))
    }, terminalUrl)
    const routed = await drawerOutcome()
    report.routedDrawer = routed
    await shot('routed-drawer')
    const routedState = await surroundings()
    check('routed-thread-retained', surroundingsHold(routedState, before, true) && routedState.drawers === 1, routedState)
    check('routed-drawer-reason', routed.kind === 'reason' && specific(routed.text) && routed.retry === 0, routed)
  }

  // Cold explicit deep link: a fresh load lands on the drawer beside the thread. Required for a
  // disabled toggle; for an enabled one it shows the same surface without the click.
  await page.goto(origin + terminalUrl)
  await header.waitFor({ state: 'visible', timeout: 90_000 })
  // A hidden thread is a finding, not a precondition failure: the checks below record it.
  await settledThread().catch(() => {})
  const direct = await drawerOutcome()
  report.directDrawer = direct
  await shot('direct-drawer')
  await pin()
  const directState = await surroundings()
  check('direct-thread-retained', surroundingsHold(directState, before, false) && directState.drawers === 1, directState)
  check('direct-drawer-reason', enabled
    ? direct.kind === 'grid' || (direct.kind === 'reason' && specific(direct.text))
    : direct.kind === 'reason' && specific(direct.text) && direct.retry === 0, direct)
  check('read-only', report.network.abortedActions.length === 0, { aborted: report.network.abortedActions })
  report.pass = report.steps.every((entry) => entry.ok)
} catch (error) {
  report.error = String(error).slice(0, 300)
  await shot('failure')
}
report.network.runtimeGetCount = report.network.runtimeGets.length
report.roster.selectedPresent = report.roster.selected.length > 0
report.roster.selectedLatest = report.roster.selected.at(-1) ?? null
fs.writeFileSync(`${out}/terminal-failure-e2e.json`, JSON.stringify(report, null, 2))
console.log('ROSTER', JSON.stringify({ present: report.roster.selectedPresent, latest: report.roster.selectedLatest, agentIdsSeen: report.roster.agentIdsSeen }))
console.log('RUNTIME_GETS', report.network.runtimeGetCount, 'ATTACHES', report.network.terminalAttaches)
console.log('PASS', report.pass)
await browser.close()
process.exit(report.pass ? 0 : 1)
