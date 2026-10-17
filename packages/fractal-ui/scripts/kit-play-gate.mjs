import { createServer } from 'node:http'
import { readFile, stat, mkdir, writeFile } from 'node:fs/promises'
import { resolve, sep, extname, join } from 'node:path'
import { performance } from 'node:perf_hooks'
import { strictStoryGuard } from './details-strict-story-guard.mjs'
import { availableParallelism, cpus, loadavg } from 'node:os'
import { storyIdsFromIndex } from './kit-play-index.mjs'
import { commitSetCases, fullCases } from './kit-play-commit-set.mjs'
import { createIsolatedKitWorker } from './kit-play-isolation.mjs'

// Own an ephemeral localhost server: missing assets remain real, standard 404s.
const [buildDirectory, outputDirectory] = process.argv.slice(2)
if (!buildDirectory || !outputDirectory) throw new Error('Usage: node kit-play-gate.mjs <storybook-static> <receipt-directory>')
// KIT_PLAY_SET=critical runs only stories tagged `critical`; KIT_PLAY_SET=commit is the per-commit
// set, a narrowing of those (see kit-play-commit-set.mjs for what it leaves out);
// KIT_PLAY_A11Y=off skips axe, which the separate full run keeps; KIT_PLAY_MAX_SECONDS
// is the set's wall-time budget: every case still runs, and a run over budget fails.
// An empty value is unset: workflow expressions yield '' for "no value".
const option = (name, allowed, fallback) => {
  const value = process.env[name] || fallback
  if (!allowed.includes(value)) throw new Error(`${name} must be one of ${allowed.join(', ')}`)
  return value
}
const set = option('KIT_PLAY_SET', ['all', 'critical', 'commit'], 'all')
const a11y = option('KIT_PLAY_A11Y', ['on', 'off'], 'on')
const maxSeconds = process.env.KIT_PLAY_MAX_SECONDS ? Number(process.env.KIT_PLAY_MAX_SECONDS) : null
if (maxSeconds !== null && !(Number.isFinite(maxSeconds) && maxSeconds > 0)) throw new Error('KIT_PLAY_MAX_SECONDS must be a positive number of seconds')
const root = resolve(buildDirectory)
const mime = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.css': 'text/css', '.json': 'application/json', '.svg': 'image/svg+xml', '.png': 'image/png', '.woff2': 'font/woff2' }
const server = createServer(async (request, response) => {
  try {
    const pathname = decodeURIComponent(new URL(request.url, 'http://localhost').pathname)
    let file = resolve(root, `.${pathname}`)
    if (file !== root && !file.startsWith(root + sep)) {
      response.writeHead(403).end('Forbidden')
      return
    }
    if ((await stat(file)).isDirectory()) file = join(file, 'index.html')
    const bytes = await readFile(file)
    // This server owns one immutable build for the job's lifetime. Context reuse
    // may cache resources, never the mutable storage belonging to a story.
    response.writeHead(200, {
      'Content-Type': mime[extname(file)] ?? 'application/octet-stream',
      'Cache-Control': 'public, max-age=31536000, immutable',
    }).end(bytes)
  } catch (error) {
    const missing = error.code === 'ENOENT' || error.code === 'ENOTDIR'
    response.writeHead(missing ? 404 : 500).end(missing ? 'Not Found' : 'Internal Server Error')
  }
})
await new Promise((accept, reject) => {
  server.once('error', reject)
  server.listen(0, '127.0.0.1', accept)
})
const base = `http://127.0.0.1:${server.address().port}`
const started = performance.now()
// Machine-wide CPU over the plays, in core-minutes, with the load it ran under:
// exact on a dedicated runner; on a shared machine read it with the load averages.
const busyMilliseconds = () => cpus().reduce((sum, { times }) => sum + times.user + times.nice + times.sys + times.irq, 0)
const busyStarted = busyMilliseconds(), loadStarted = loadavg()
const rows = []
const workers = Math.min(6, availableParallelism())
let ids = [], expectedCases = 0, omitted = { stories: [], cases: 0 }, browser, fatalError
await mkdir(outputDirectory, { recursive: true })
const writeReceipt = async () => {
  const elapsedSeconds = (performance.now() - started) / 1000
  const phaseTotals = new Map()
  for (const row of rows) {
    for (const [name, value] of Object.entries(row.timings ?? {})) {
      if (!Number.isFinite(value)) continue
      const previous = phaseTotals.get(name) ?? { sum: 0, count: 0 }
      phaseTotals.set(name, { sum: previous.sum + value, count: previous.count + 1 })
    }
  }
  const phaseMeanMilliseconds = Object.fromEntries([...phaseTotals].map(([name, { sum, count }]) => [name, sum / count]))
  const overBudget = maxSeconds !== null && elapsedSeconds > maxSeconds
  const receipt = {
    pass: !fatalError && !overBudget && ids.length > 0 && rows.length === expectedCases && rows.every(row => row.pass),
    set, a11y, maxSeconds, overBudget,
    stories: ids.length, expectedCases, cases: rows.length, workers, omitted,
    isolation: 'reused-context-fresh-page-cleared-storage',
    cpu: { count: cpus().length, busyCoreMinutes: (busyMilliseconds() - busyStarted) / 60000, loadavgStart: loadStarted, loadavgEnd: loadavg() },
    elapsedSeconds, fatalError, rows: rows.toSorted((a, b) => a.id.localeCompare(b.id) || a.scheme.localeCompare(b.scheme) || a.width - b.width),
    effectiveWorkerSecondsPerCase: rows.length ? elapsedSeconds * workers / rows.length : null,
    meanCaseSeconds: rows.length ? rows.reduce((sum, row) => sum + row.elapsedSeconds, 0) / rows.length : null,
    phaseMeanMilliseconds,
  }
  await writeFile(join(outputDirectory, 'receipt.json'), JSON.stringify(receipt, null, 2) + '\n')
  const { rows: _rows, ...summary } = receipt
  console.log(`KIT_PLAY_SUMMARY ${JSON.stringify(summary)}`)
  if (overBudget) console.error(`Kit plays (${set}) took ${elapsedSeconds.toFixed(1)} s, over the ${maxSeconds} s budget: fix the slow cases at their cause; the budget is not raised.`)
  return receipt
}
try {
  const response = await fetch(`${base}/index.json`)
  if (!response.ok) throw new Error(`Storybook index: HTTP ${response.status}`)
  ids = storyIdsFromIndex(await response.json(), set === 'all' ? {} : { tag: 'critical' })
  let cases
  if (set === 'commit') {
    const selection = commitSetCases(ids)
    ;({ ids, cases } = selection)
    omitted = { stories: selection.omittedStories, cases: selection.omittedCases }
  } else {
    cases = fullCases(ids)
  }
  expectedCases = cases.length
  const { chromium } = await import(process.env.PLAYWRIGHT_MODULE ?? 'playwright')
  browser = await chromium.launch(process.env.CHROMIUM_PATH ? { executablePath: process.env.CHROMIUM_PATH } : {})
  let nextCase = 0
  const runCase = async (worker, { id, scheme, width }) => {
    const caseStarted = performance.now()
    const { resetMilliseconds } = await worker.reset()
    const createStarted = performance.now()
    const page = await worker.newPage({ viewport: { width, height: 900 }, colorScheme: scheme })
    const pageCreated = performance.now()
    const guard = await strictStoryGuard(page, base)
    const guardReady = performance.now()
    const row = {
      id, scheme, width, height: 900, pass: false,
      timings: {
        resetMilliseconds, pageCreateMilliseconds: pageCreated - createStarted,
        guardMilliseconds: guardReady - pageCreated,
      },
    }
    let domLoaded
    page.once('domcontentloaded', () => { domLoaded = performance.now() })
    try {
      // The guard awaits strict storyFinished; unrelated network idleness is not play readiness.
      // a11y.manual is addon-a11y's own switch for its afterEach axe run.
      await page.goto(`${base}/iframe.html?id=${encodeURIComponent(id)}&viewMode=story&args=${encodeURIComponent(`scheme:${scheme}`)}${a11y === 'off' ? `&globals=${encodeURIComponent('a11y.manual:!true')}` : ''}`, { waitUntil: 'domcontentloaded' })
      const storyReady = performance.now()
      row.timings.navigationToDomMilliseconds = domLoaded - guardReady
      row.timings.readinessAfterDomMilliseconds = storyReady - domLoaded
      await guard.check()
      if (!guard.records.every(record => record.finished.every(event => event.payload.storyId === id))) {
        throw new Error('storyFinished evidence belongs to a different story')
      }
      // With a11y on, every finished story carries a passed axe report; with it off, none.
      for (const { payload } of guard.records.flatMap(record => record.finished)) {
        const reports = (payload.reporters ?? []).filter(report => report.type === 'a11y')
        if (a11y === 'on' && !(reports.length === 1 && reports[0].status === 'passed')) throw new Error(`a11y on: expected one passed a11y report, got ${JSON.stringify(reports)}`)
        if (a11y === 'off' && reports.length) throw new Error(`a11y off: axe still ran: ${JSON.stringify(reports)}`)
      }
      row.timings.assertionsMilliseconds = performance.now() - storyReady
      row.pass = true
    } catch (error) {
      row.error = String(error)
      row.events = await page.evaluate(() => window.__dt10StrictEvents ?? []).catch(() => [])
    } finally {
      row.strict = guard.records
      // Keep the original elapsed definition (before page.close) for comparison.
      row.elapsedSeconds = (performance.now() - caseStarted) / 1000
      const closeStarted = performance.now()
      try {
        await page.close()
      } catch (error) {
        row.pass = false
        row.error = `${row.error ?? ''} page cleanup: ${error}`
        throw error
      } finally {
        row.timings.closeMilliseconds = performance.now() - closeStarted
        rows.push(row)
        console.log(JSON.stringify(row))
      }
    }
  }
  const results = await Promise.allSettled(Array.from({ length: workers }, async () => {
    const worker = await createIsolatedKitWorker(browser, base)
    try {
      while (nextCase < cases.length) await runCase(worker, cases[nextCase++])
    } finally {
      await worker.close()
    }
  }))
  const rejected = results.filter(result => result.status === 'rejected')
  if (rejected.length) throw new AggregateError(rejected.map(result => result.reason), 'Kit play workers failed')
} catch (error) {
  fatalError = String(error)
} finally {
  await browser?.close()
  await new Promise(resolve => server.close(resolve))
}
const receipt = await writeReceipt()
// The complete receipt stays in CI logs even when the gate fails.
console.log(JSON.stringify(receipt))
if (!receipt.pass) process.exitCode = 1
