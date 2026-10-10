#!/usr/bin/env node
/** Synthetic, production-only switch gate. No gateway, credentials or saved browser state.
 * node apps/fractal-web/scripts/switch-budget-proof.mjs [--baseline] [--turns=100]
 * --baseline records failing budgets without making the process fail. Default enforces B2/B3.
 */
import { spawn } from 'node:child_process'
import { mkdir, mkdtemp, rm } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { build, preview, loadConfigFromFile } from 'vite'
const app = fileURLToPath(new URL('..', import.meta.url)).replace(/\/$/, '')
const scratchRoot = fileURLToPath(new URL('../../../tmp', import.meta.url))
const session = `switch-budget-${Date.now()}`
const turns = Number(process.argv.find(arg => arg.startsWith('--turns='))?.split('=')[1] ?? 100)
// One turn measures the shell and harness floor; 50-200 are the realistic transcript sizes.
if (!Number.isInteger(turns) || turns < 1 || turns > 200) throw new Error('Use 1 to 200 turns')
const baseline = process.argv.includes('--baseline')
const regression = process.argv.find(arg => arg.startsWith('--regression='))?.slice('--regression='.length)
const isRegressionRun = regression === 'notice' || regression === 'keyboard'
if (regression !== undefined && !isRegressionRun) throw new Error('Use --regression=notice or --regression=keyboard')
await mkdir(scratchRoot, { recursive: true })
const scratch = await mkdtemp(`${scratchRoot}/switch-budget-`)
const run = args => new Promise((resolve, reject) => {
  const child = spawn('playwright-cli', [`-s=${session}`, ...args], { cwd: scratch, stdio: ['ignore', 'pipe', 'inherit'] })
  let output = ''
  child.stdout.on('data', chunk => { output += chunk })
  child.on('error', reject)
  child.on('exit', code => code === 0 ? resolve(output) : reject(new Error(output)))
})
let server
try {
  const loaded = await loadConfigFromFile({ command: 'build', mode: 'production' }, `${app}/vite.config.ts`, app)
  const fixture = {
    name: 'switch-budget-synthetic-entry', enforce: 'pre',
    transform(code, id) {
      if (id === `${app}/src/web/main.tsx`) return `import '../../scripts/switch-budget-fixture.tsx'`
    },
    transformIndexHtml(html) { return html.replace(/<script data-wf-early-connect>[\s\S]*?<\/script>/, '') },
    configurePreviewServer(previewServer) {
      previewServer.middlewares.use((req, res, next) => {
        if (!req.url.startsWith('/switch-page?')) return next()
        setTimeout(() => { res.setHeader('content-type', 'text/plain'); res.end('synthetic frame') }, 120)
      })
    },
  }
  const config = { ...loaded.config, configFile: false, root: `${app}/src/web`,
    plugins: [fixture, ...loaded.config.plugins.flat().filter(plugin => plugin?.name !== 'wf:shared-client-gateway')],
    build: { ...loaded.config.build, outDir: `${scratch}/dist`, minify: false },
    preview: { host: '127.0.0.1', port: 0, strictPort: false },
  }
  await build(config)
  server = await preview(config)
  const port = server.httpServer.address().port
  await run(['open', 'about:blank'])
  const output = await run(['run-code', `async page => {
    const errors = []
    page.on('pageerror', error => errors.push(error.message))
    await page.setViewportSize({ width: 1440, height: 1000 })
    await page.goto('http://127.0.0.1:${port}/?turns=${turns}')
    await page.locator('[data-testid="transcript-turn"]').first().waitFor().catch(async error => { throw new Error(error.message + '\\n' + errors.join('\\n') + '\\n' + await page.locator('body').innerText()) })
    const regression = ${JSON.stringify(regression) ?? 'undefined'}
    if (regression === 'notice') {
      for (const [target, text] of [['monitor/quota', 'View unavailable.'], ['terminal/example', 'Terminal unavailable.']]) {
        await page.evaluate(target => {
          history.pushState(null, '', '/w/agent/switch-alpha?open=' + encodeURIComponent(target + ':detail'))
          dispatchEvent(new PopStateEvent('popstate'))
        }, target)
        const notice = page.getByRole('status').filter({ hasText: text })
        await notice.waitFor()
        // CSS visibility alone misses opaque siblings overpainting a notice. Hit-testing
        // the text centre proves the foreground element is exposed with compiled StyleX.
        if (!await notice.evaluate(node => {
          const rect = node.getBoundingClientRect()
          return node.contains(document.elementFromPoint(rect.x + rect.width / 2, rect.y + rect.height / 2))
        })) throw new Error('Unavailable notice is covered: ' + target)
      }
      return 'RECEIPT ' + JSON.stringify({ regression, passed: true })
    }
    if (regression === 'keyboard') {
      await page.locator('nav[aria-label="Agent roster"] button[aria-label^="Switch Beta,"]').click()
      await page.waitForFunction(() => [...document.querySelectorAll('[data-testid="transcript-header"]')].some(node => node.textContent.includes('Switch Beta') && node.parentElement.querySelector('[data-testid="transcript-turn"]')))
      await page.locator('nav[aria-label="Agent roster"] button[aria-label^="Switch Alpha,"]').click()
      await page.waitForTimeout(100)
      const field = page.locator('textarea').first()
      await field.fill('The outgoing draft must not be sent.')
      await field.focus()
      await page.evaluate(() => {
        window.savedFrame = requestAnimationFrame
        window.heldFrames = []
        window.requestAnimationFrame = callback => { window.heldFrames.push(callback); return 0 }
        history.pushState(null, '', '/w/agent/switch-beta')
        dispatchEvent(new PopStateEvent('popstate'))
      })
      await page.waitForTimeout(30)
      const focused = await field.evaluate(node => node === document.activeElement)
      await field.evaluate(node => node.focus())
      const refocused = await field.evaluate(node => node === document.activeElement)
      const ax = await page.locator('section[aria-label="Agent workspace"]').ariaSnapshot()
      await page.keyboard.press('Enter')
      const sends = await page.evaluate(() => performance.getEntriesByType('mark').filter(entry => entry.name.startsWith('synthetic-send:')).length)
      if (focused || refocused || sends !== 0 || (ax.match(/textbox "Message"/g) ?? []).length !== 1) {
        throw new Error('Covered composer remains interactive: ' + JSON.stringify({ focused, refocused, sends, ax }))
      }
      await page.evaluate(() => { window.requestAnimationFrame = window.savedFrame; for (const callback of window.heldFrames) requestAnimationFrame(callback) })
      return 'RECEIPT ' + JSON.stringify({ regression, passed: true })
    }
    await page.evaluate(() => {
      window.switchSamples = []
      let selection
      // checkVisibility excludes display:none and content-visibility:hidden ancestors.
      const visible = node => node instanceof HTMLElement && node.checkVisibility({ contentVisibilityAuto: true })
      // Checked once per frame, before that frame paints; the MessageChannel task then runs
      // after the paint which first shows the selected transcript's committed turns.
      const check = selected => {
        if (selected !== selection) return
        const lane = [...document.querySelectorAll('[data-testid="transcript-scroll"]')].find(node => {
          for (let parent = node.parentElement; parent; parent = parent.parentElement) {
            const header = parent.querySelector('[data-testid="transcript-header"]')
            if (header) return header.textContent.includes(selected.name) && visible(node)
          }
          return false
        })
        if (!lane || lane.querySelector('[data-testid="transcript-placeholder"]') || !lane.querySelector('[data-testid="transcript-turn"]')) {
          requestAnimationFrame(() => check(selected))
          return
        }
        const shownMs = performance.now() - selected.start
        const channel = new MessageChannel()
        channel.port1.onmessage = () => {
          channel.port1.close(); channel.port2.close()
          const request = performance.getEntriesByName('switch-request:' + selected.ref).at(-1)?.startTime
          const frame = performance.getEntriesByName('switch-frame:' + selected.ref).at(-1)?.startTime
          window.switchSamples.push({ name: selected.name, ms: performance.now() - selected.start, shownInFrameMs: shownMs, committedBeforeClick: selected.committedBeforeClick,
            ...(request >= selected.start ? { clickToRequestMs: request - selected.start, requestToFrameMs: frame - request, frameToPaintMs: performance.now() - frame } : {}) })
        }
        channel.port2.postMessage(null)
      }
      document.addEventListener('click', event => {
        const row = event.target.closest('button')
        if (!row || !row.closest('nav[aria-label="Agent roster"]')) return
        const name = ['Switch Alpha', 'Switch Beta', 'Switch Gamma'].find(name => row.getAttribute('aria-label')?.startsWith(name + ','))
        if (!name) return
        const ref = 'agent/switch-' + name.split(' ')[1].toLowerCase()
        // The roster commits on press; a URL already naming the target would mean the click
        // started the clock after the switch, so the sample is rejected below.
        selection = { name, ref, start: performance.now(), committedBeforeClick: decodeURIComponent(location.pathname) === '/w/' + ref }
        const selected = selection
        requestAnimationFrame(() => check(selected))
      }, true)
    })
    const switchTo = async name => {
      const n = await page.evaluate(() => window.switchSamples.length)
      await page.locator('nav[aria-label="Agent roster"] button[aria-label^="' + name + ',"]').click()
      await page.waitForFunction(n => window.switchSamples.length > n, n).catch(async error => { throw new Error(error.message + '\\n' + await page.evaluate(() => JSON.stringify({ url: location.href, lanes: [...document.querySelectorAll('[data-testid="transcript-scroll"]')].map(node => ({ parent: node.parentElement?.outerHTML.slice(0, 1000), turns: node.querySelectorAll('[data-testid="transcript-turn"]').length })) }))) })
    }
    // Two first opens (B3), then nine untraced switches back across three retained agents (B2).
    await switchTo('Switch Beta')
    await switchTo('Switch Gamma')
    for (let i = 0; i < 9; i++) await switchTo(['Switch Alpha', 'Switch Beta', 'Switch Gamma'][i % 3])
    const samples = await page.evaluate(() => window.switchSamples)
    // A separate traced pass explains the cost; profiler overhead never enters the budget samples.
    const cdp = await page.context().newCDPSession(page)
    const events = []
    cdp.on('Tracing.dataCollected', data => events.push(...data.value))
    await cdp.send('Tracing.start', { categories: 'devtools.timeline,v8,disabled-by-default-v8.cpu_profiler', options: 'sampling-frequency=1000' })
    for (let i = 0; i < 6; i++) await switchTo(['Switch Alpha', 'Switch Beta', 'Switch Gamma'][i % 3])
    const completed = new Promise(resolve => cdp.once('Tracing.tracingComplete', resolve))
    await cdp.send('Tracing.end'); await completed
    const totals = {}
    for (const event of events) if (event.ph === 'X') totals[event.name] = (totals[event.name] ?? 0) + (event.dur ?? 0) / 1000
    // Element counts show whether a switch restyles the revealed transcript or only the shell.
    const styleRecalcs = events.filter(event => event.ph === 'X' && event.name === 'UpdateLayoutTree').sort((a, b) => b.dur - a.dur).slice(0, 8)
      .map(event => ({ ms: Math.round(event.dur / 100) / 10, elements: event.args?.elementCount ?? event.args?.data?.elementCount }))
    const nodes = new Map(), cpu = {}, stacks = {}
    // Minified anonymous frames keep their bundle and column, which source maps resolve.
    const label = node => node?.callFrame.functionName || (node?.callFrame.url ? '(anonymous ' + node.callFrame.url.split('/').at(-1) + ':' + node.callFrame.lineNumber + ':' + node.callFrame.columnNumber + ')' : '(anonymous)')
    for (const event of events) {
      const profile = event.args?.data?.cpuProfile
      if (!profile) continue
      for (const node of profile.nodes ?? []) nodes.set(node.id, node)
      for (let i = 0; i < (profile.samples?.length ?? 0); i++) {
        const node = nodes.get(profile.samples[i]), ms = (event.args.data.timeDeltas?.[i] ?? 1000) / 1000
        cpu[label(node)] = (cpu[label(node)] ?? 0) + ms
        // Caller chains attribute host work (style, DOM) to the app or kit frame that caused it.
        const chain = []
        for (let current = node; current && chain.length < 6; current = nodes.get(current.parent)) chain.push(label(current))
        const key = chain.join(' < ')
        if (!/^\\((program|idle|garbage collector|root|anonymous)\\)/.test(key)) stacks[key] = (stacks[key] ?? 0) + ms
      }
    }
    if (samples.some(sample => sample.committedBeforeClick)) throw new Error('A switch committed before its click; the clock would start late')
    const warm = samples.slice(2).map(sample => Math.round(sample.ms * 10) / 10)
    return 'RECEIPT ' + JSON.stringify({ turns: ${turns}, cold: samples.slice(0, 2), warm, styleRecalcs,
      warmShownInFrameMs: samples.slice(2).map(sample => Math.round(sample.shownInFrameMs * 10) / 10),
      warmMedianMs: [...warm].sort((a, b) => a - b)[Math.floor(warm.length / 2)],
      timelineMs: Object.fromEntries(Object.entries(totals).sort((a,b) => b[1]-a[1]).slice(0,20)),
      cpuSelfMs: Object.fromEntries(Object.entries(cpu).sort((a,b) => b[1]-a[1]).slice(0,30)),
      cpuStacksMs: Object.fromEntries(Object.entries(stacks).sort((a,b) => b[1]-a[1]).slice(0,15)) })
  }`])
  const receipt = JSON.parse(output.match(/RECEIPT (.+)/)?.[1]?.replace(/"$/, '').replace(/\\"/g, '"') ?? 'null')
  if (!receipt) throw new Error(output)
  console.log(JSON.stringify(receipt, null, 2))
  // B2: switching back paints within 50 ms (median of nine). B3: a first open, including the
  // synthetic 120 ms server wait, paints within 300 ms.
  if (!isRegressionRun && !baseline && (receipt.warmMedianMs >= 50 || receipt.cold.some(sample => sample.ms >= 300))) throw new Error('Switch budget exceeded')
} finally {
  await run(['close']).catch(() => {})
  await new Promise(resolve => server ? server.httpServer.close(resolve) : resolve())
  await rm(scratch, { recursive: true, force: true })
}
