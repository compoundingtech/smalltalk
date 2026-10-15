import { createServer } from 'node:http'
import { readFile, stat, mkdir, writeFile, appendFile } from 'node:fs/promises'
import { resolve, sep, extname, join } from 'node:path'
import { performance } from 'node:perf_hooks'
import { strictStoryGuard } from './details-strict-story-guard.mjs'
import { availableParallelism } from 'node:os'
import { storyIdsFromIndex } from './kit-play-index.mjs'
import { createIsolatedKitWorker } from './kit-play-isolation.mjs'
import { shardStories } from './kit-play-shards.mjs'

// Own an ephemeral localhost server: missing assets remain real, standard 404s.
const [buildDirectory, outputDirectory] = process.argv.slice(2)
if (!buildDirectory || !outputDirectory) throw new Error('Usage: node kit-play-gate.mjs <storybook-static> <receipt-directory>')
const shardIndex = Number(process.env.KIT_PLAY_SHARD_INDEX ?? 0)
const shardCount = Number(process.env.KIT_PLAY_SHARD_COUNT ?? 1)
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
const rows = []
const workers = Math.min(6, availableParallelism())
let ids = [], fullIds = [], browser, fatalError
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
  const receipt = {
    pass: !fatalError && fullIds.length > 0 && rows.length === ids.length * 4 && rows.every(row => row.pass),
    shardIndex, shardCount, fullIds,
    runId: process.env.GITHUB_RUN_ID, runAttempt: Number(process.env.GITHUB_RUN_ATTEMPT),
    headSha: process.env.GITHUB_SHA,
    stories: ids.length, expectedCases: ids.length * 4, cases: rows.length, workers,
    passed: rows.filter(row => row.pass).length, failed: rows.filter(row => !row.pass).length,
    playMinutes: elapsedSeconds / 60,
    storybookBuildSeconds: Number(process.env.KIT_STORYBOOK_BUILD_SECONDS ?? 0),
    isolation: 'reused-context-fresh-page-cleared-storage',
    elapsedSeconds, fatalError, rows: rows.toSorted((a, b) => a.id.localeCompare(b.id) || a.scheme.localeCompare(b.scheme) || a.width - b.width),
    effectiveWorkerSecondsPerCase: rows.length ? elapsedSeconds * workers / rows.length : null,
    meanCaseSeconds: rows.length ? rows.reduce((sum, row) => sum + row.elapsedSeconds, 0) / rows.length : null,
    phaseMeanMilliseconds,
  }
  await writeFile(join(outputDirectory, 'receipt.json'), JSON.stringify(receipt, null, 2) + '\n')
  const { rows: _rows, ...summary } = receipt
  console.log(`KIT_PLAY_SUMMARY ${JSON.stringify(summary)}`)
  if (process.env.GITHUB_STEP_SUMMARY && process.env.KIT_PLAY_SHARD_COUNT) {
    await appendFile(process.env.GITHUB_STEP_SUMMARY, `\n| Shard | Cases (completed/expected) | Passed | Failed | Play minutes | Result |\n| --- | --- | --- | --- | --- | --- |\n| kit ${shardIndex}/${shardCount} | ${receipt.cases}/${receipt.expectedCases} | ${receipt.passed} | ${receipt.failed} | ${receipt.playMinutes.toFixed(2)} | ${receipt.pass ? 'PASS' : 'FAIL'} |\n`)
    await appendFile(process.env.GITHUB_STEP_SUMMARY, `\nkit ${shardIndex}/${shardCount} Storybook build: ${(receipt.storybookBuildSeconds / 60).toFixed(2)} runner-minutes (included in this shard; duplicate build cost is the sum of nonzero-shard builds).\n`)
  }
  return receipt
}
try {
  const response = await fetch(`${base}/index.json`)
  if (!response.ok) throw new Error(`Storybook index: HTTP ${response.status}`)
  // Every shard validates the complete Storybook contract before selecting work.
  fullIds = storyIdsFromIndex(await response.json()).toSorted()
  ids = shardStories(fullIds, shardIndex, shardCount)
  const { chromium } = await import(process.env.PLAYWRIGHT_MODULE ?? 'playwright')
  browser = await chromium.launch(process.env.CHROMIUM_PATH ? { executablePath: process.env.CHROMIUM_PATH } : {})
  const cases = ids.flatMap(id => ['dark', 'light'].flatMap(scheme => [440, 1440].map(width => ({ id, scheme, width }))))
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
      await page.goto(`${base}/iframe.html?id=${encodeURIComponent(id)}&viewMode=story&args=${encodeURIComponent(`scheme:${scheme}`)}`, { waitUntil: 'domcontentloaded' })
      const storyReady = performance.now()
      row.timings.navigationToDomMilliseconds = domLoaded - guardReady
      row.timings.readinessAfterDomMilliseconds = storyReady - domLoaded
      await guard.check()
      if (!guard.records.every(record => record.finished.every(event => event.payload.storyId === id))) {
        throw new Error('storyFinished evidence belongs to a different story')
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
