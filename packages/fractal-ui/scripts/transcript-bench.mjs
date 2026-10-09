async (page) => {
  // One run of the transcript switch budget over 50/100/200 turns (Fractal UI / Transcript Bench).
  // B3 traces a first open: `bench:mount` to the end of the first painted frame (`Commit`) that shows turns.
  // B2 traces a switch back: revealing the retained `content-visibility: hidden` pane to its first painted frame,
  // after every turn is in the DOM.
  // Run it with `playwright-cli run-code --filename scripts/transcript-bench.mjs`; the session must already be on
  // the Storybook origin. It prints one JSON array with a row per transcript size.
  const origin = new URL(page.url()).origin
  const sizes = [50, 100, 200]
  const categories = ['devtools.timeline', 'disabled-by-default-devtools.timeline', 'blink.user_timing', 'v8.execute'].join(',')
  const phases = ['UpdateLayoutTree', 'Layout', 'PrePaint', 'Paint', 'Layerize', 'FunctionCall', 'EvaluateScript', 'v8.callFunction', 'RunMicrotasks', 'ResizeObserver']
  const round = value => Math.round(value * 10) / 10
  const trace = async (mark, end, action) => {
    const cdp = await page.context().newCDPSession(page)
    const events = []
    cdp.on('Tracing.dataCollected', ({ value }) => events.push(...value))
    const complete = new Promise(resolve => cdp.once('Tracing.tracingComplete', resolve))
    await cdp.send('Tracing.start', { categories, transferMode: 'ReportEvents' })
    await action()
    await page.waitForTimeout(800)
    await cdp.send('Tracing.end')
    await complete
    await cdp.detach()
    const userMark = name => events.filter(event => event.name === name && event.cat.includes('blink.user_timing')).sort((a, b) => a.ts - b.ts).at(-1)
    const start = userMark(mark)
    if (start === undefined) throw new Error(`Trace is missing the ${mark} mark`)
    const from = end === undefined ? start : userMark(end)
    if (from === undefined) throw new Error(`Trace is missing the ${end} mark`)
    const main = event => event.pid === start.pid && event.tid === start.tid
    const commit = events.filter(event => main(event) && event.name === 'Commit' && event.ph === 'X' && event.ts >= from.ts).sort((a, b) => a.ts - b.ts)[0]
    if (commit === undefined) throw new Error(`Trace has no main-thread Commit after ${end ?? mark}`)
    const stop = commit.ts + commit.dur
    const breakdown = {}
    for (const event of events) {
      if (!main(event) || event.ph !== 'X' || event.ts < start.ts || event.ts + (event.dur ?? 0) > stop || !phases.includes(event.name)) continue
      breakdown[event.name] = round((breakdown[event.name] ?? 0) + event.dur / 1000)
    }
    // The longest main-thread task inside the interval: for B3 the React render and commit of the visible turns.
    const longestTaskMs = round(Math.max(0, ...events.filter(event => main(event) && event.name === 'RunTask' && event.ph === 'X' && event.ts + event.dur >= start.ts && event.ts <= stop).map(event => event.dur / 1000)))
    return { ms: round((stop - start.ts) / 1000), longestTaskMs, breakdown }
  }
  const results = []
  await page.setViewportSize({ width: 1440, height: 900 })
  for (const turns of sizes) {
    await page.goto(`${origin}/iframe.html?id=fractal-ui-transcript-bench--turns-${turns}&viewMode=story`)
    await page.waitForFunction(() => window.__transcriptBench !== undefined, null, { timeout: 120000 })
    await page.evaluate(() => window.__transcriptBench.settled())
    const nodes = await page.evaluate(() => document.querySelector('[data-testid="bench-pane"]').querySelectorAll('*').length)
    // Grammar and module caches are warm after the story's own mount; the traced mount is a fresh commit.
    await page.evaluate(() => window.__transcriptBench.unmount())
    await page.waitForTimeout(300)
    const b3 = await trace('bench:mount', 'bench:turns', () => page.evaluate(() => window.__transcriptBench.mount()))
    await page.evaluate(() => window.__transcriptBench.settled())
    b3.allTurnsMs = await page.evaluate(() => {
      const [start] = performance.getEntriesByName('bench:mount').slice(-1)
      const [all] = performance.getEntriesByName('bench:all-turns').slice(-1)
      return all === undefined || start === undefined ? null : Math.round((all.startTime - start.startTime) * 10) / 10
    })
    const after = await page.evaluate(() => {
      const scroll = document.querySelector('[data-testid="transcript-scroll"]')
      const turn = document.querySelector('[data-testid="transcript-turn"]')
      // Records which variant the preview served: the turn's own containment.
      return { gap: scroll.scrollHeight - scroll.clientHeight - scroll.scrollTop, turns: document.querySelectorAll('[data-testid="transcript-turn"]').length, containment: getComputedStyle(turn).contentVisibility }
    })
    await page.evaluate(() => window.__transcriptBench.hide())
    await page.waitForTimeout(300)
    const b2 = await trace('bench:reveal', undefined, () => page.evaluate(() => window.__transcriptBench.reveal()))
    results.push({ turns, nodes, containment: after.containment, mountedTurns: after.turns, followGapAfterBackfill: after.gap, b3, b2 })
  }
  return JSON.stringify(results)
}
