// Run with: playwright-cli -s=h004-sidebar-proof --raw run-code --filename ./sidebar-search-probe.cjs
// H004: capture-phase input -> second rAF; six rounds x five unique nonempty keystrokes.
// Each invocation owns and closes one fresh browser context: profiling hooks never accumulate.
async (inputPage) => {
  const context = await inputPage.context().browser().newContext({ viewport: { width: 1440, height: 1000 } });
  const page = await context.newPage();
  try {
  await page.addInitScript(() => {
    const hook = window.__REACT_DEVTOOLS_GLOBAL_HOOK__ ??= {
      supportsFiber: true, renderers: new Map(),
      inject(renderer) { const id = this.renderers.size + 1; this.renderers.set(id, renderer); return id; },
      onCommitFiberRoot() {}, onCommitFiberUnmount() {}, checkDCE() {},
    };
    const original = hook.onCommitFiberRoot;
    window.__sidebarReactCommits = [];
    let previousFibers = new WeakSet();
    hook.onCommitFiberRoot = function (...args) {
      original?.apply(this, args);
      const counts = {}, treeDurations = [];
      const nextFibers = new WeakSet();
      const visit = (fiber) => {
        if (!fiber) return;
        nextFibers.add(fiber);
        const type = fiber.type;
        const name = typeof type === 'function' ? type.displayName ?? type.name : type?.displayName ?? type?.render?.name ?? type?.type?.name;
        const label = name?.includes('TreeInner') ? 'TreeInner' : name;
        if (label === 'TreeInner' && Number.isFinite(fiber.actualDuration)) {
          window.__sidebarProfilerAvailable = true;
          if (!previousFibers.has(fiber)) treeDurations.push(fiber.actualDuration);
        }
        if (!previousFibers.has(fiber) && (fiber.flags & 1) !== 0 && label && /Sidebar|AgentFolders|AgentFolderTree|LiveAgentWorkspace|TreeInner/.test(label)) counts[label] = (counts[label] ?? 0) + 1;
        visit(fiber.child); visit(fiber.sibling);
      };
      visit(args[1].current);
      previousFibers = nextFibers;
      window.__sidebarReactCommits.push({ at: performance.now(), counts, treeDurations });
    };
  });
  await page.goto(inputPage.url(), { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('[data-wf-agent-ref]', { timeout: 90000 });
  await page.waitForTimeout(2000);
  await page.evaluate(() => {
    window.__h004InputEvents = [];
    window.__h004InputListener = (event) => {
      if (event.target.getAttribute('aria-label') !== 'Search agents') return;
      const start = performance.now(), value = event.target.value;
      requestAnimationFrame(() => requestAnimationFrame(() => window.__h004InputEvents.push({
        start, label: 'Search agents', value, latency: performance.now() - start,
        rows: document.querySelectorAll('[data-wf-agent-ref]').length,
      })));
    };
    document.addEventListener('input', window.__h004InputListener, true);
  });
  const field = page.locator('[aria-label="Search agents"]:visible');
  for (let round = 0; round < 6; round++) {
    await field.fill('');
    await field.pressSequentially('vista', { delay: 400 });
    await page.waitForTimeout(800);
    await field.fill('');
    await page.waitForTimeout(800);
  }
  const result = await page.evaluate(() => {
    document.removeEventListener('input', window.__h004InputListener, true);
    const events = window.__h004InputEvents.filter(event => event.value !== '');
    if (events.length !== 30) throw new Error(`Expected 30 H004 keystrokes, received ${events.length}`);
    const quantile = (values, p) => [...values].sort((a, b) => a - b)[Math.floor((values.length - 1) * p)];
    const samples = events.map(event => {
      const next = window.__h004InputEvents.find(candidate => candidate.start > event.start);
      const commits = window.__sidebarReactCommits.filter(commit => commit.at >= event.start && commit.at < (next?.start ?? event.start + 400));
      const renderCounts = {}, treeDurations = [];
      for (const commit of commits) {
        for (const [name, count] of Object.entries(commit.counts)) renderCounts[name] = (renderCounts[name] ?? 0) + count;
        treeDurations.push(...commit.treeDurations);
      }
      return { ...event, renderCounts, treeRenderMs: treeDurations.reduce((sum, duration) => sum + duration, 0) };
    });
    if (!window.__sidebarProfilerAvailable) throw new Error('React tree profiling unavailable; refusing measured-zero result');
    const result = {
      url: location.href, utc: new Date().toISOString(), n: samples.length,
      method: 'H004 capture-phase input -> second rAF; 400ms spacing; floor((n-1)*p); React dev profiling hook, PerformedWork counts',
      p50: quantile(samples.map(sample => sample.latency), .5), p90: quantile(samples.map(sample => sample.latency), .9),
      treeRenderP50: quantile(samples.map(sample => sample.treeRenderMs), .5), treeRenderP90: quantile(samples.map(sample => sample.treeRenderMs), .9),
      renderCounts: samples.reduce((totals, sample) => { for (const [name, count] of Object.entries(sample.renderCounts)) totals[name] = (totals[name] ?? 0) + count; return totals; }, {}),
      samples,
    };
    window.__h004SidebarResult = result;
    return result;
  });
  const clockControl = page.getByRole('button', { name: 'Advance fixture clock', exact: true });
  if (await clockControl.count() === 1) {
    const firstCommit = await page.evaluate(() => window.__sidebarReactCommits.length);
    for (let tick = 0; tick < 5; tick++) { await clockControl.click(); await page.waitForTimeout(100); }
    result.clockCheck = await page.evaluate(first => {
      const counts = {};
      for (const commit of window.__sidebarReactCommits.slice(first)) for (const [name, count] of Object.entries(commit.counts)) counts[name] = (counts[name] ?? 0) + count;
      return { ticks: 5, renderCounts: counts, collectionRebuilt: (counts.BoundAgentFolders ?? 0) > 0 || (counts.SidebarTree ?? 0) > 0 };
    }, firstCommit);
  }
  return result;
  } finally { await context.close(); }
}
