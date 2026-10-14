// Owned 78-agent fixture: run via playwright-cli --raw run-code --filename this file.
async (inputPage) => {
  const context = await inputPage.context().browser().newContext({ viewport: { width: 1440, height: 1000 } });
  const page = await context.newPage();
  const checks = [];
  try {
    await page.goto(inputPage.url(), { waitUntil: 'domcontentloaded' });
    await page.waitForSelector('[data-wf-agent-ref]');
    const field = page.getByRole('textbox', { name: 'Search agents', exact: true });
    const tree = page.getByRole('treegrid', { name: 'Agents and folders' });
    const selected = page.locator('[role="row"]').filter({ has: page.locator('[data-wf-agent-ref="agent/benchmark-77"]') });
    await field.fill('vista agent 77');
    await selected.waitFor({ state: 'visible' });
    if (await tree.locator('[data-testid="taste-agent-row"]').count() !== 1) throw new Error('Search failed to filter to the requested identity');
    if (await selected.locator('[data-row-column="title-text"] mark').count() === 0) throw new Error('Search failed to highlight the visible name');
    checks.push('Filter identity and query highlighting');
    await field.press('ArrowDown');
    await page.keyboard.press('ArrowDown');
    await page.keyboard.press('Enter');
    await selected.locator('[aria-current="page"]').waitFor();
    checks.push('ArrowDown enters the tree and Enter selects the agent');
    await field.focus();
    await field.press('Escape');
    await page.waitForFunction(() => document.querySelector('[aria-label="Search agents"]').value === '');
    await page.waitForFunction(() => {
      const row = document.querySelector('[role="row"] [data-wf-agent-ref="agent/benchmark-77"]');
      const tree = document.querySelector('[role="treegrid"]');
      if (!row || !tree) return false;
      const rect = row.getBoundingClientRect(), viewport = tree.getBoundingClientRect();
      return rect.top >= viewport.top - 1 && rect.bottom <= viewport.bottom + 1;
    });
    checks.push('Escape clears search and reveals the selected deep row');
    await selected.locator('[data-testid="taste-agent-row"]').click({ button: 'right' });
    await page.getByRole('menu', { name: 'Actions for Vista Agent 77', exact: true }).waitFor();
    if (!await page.getByRole('menuitem', { name: 'Open conversation', exact: true }).isEnabled()) throw new Error('Context menu lost its open action');
    await page.keyboard.press('Escape');
    checks.push('Row context menu retains its conversation action');
    await tree.hover();
    await page.mouse.wheel(0, -10000);
    await page.waitForFunction(() => document.querySelector('[role="treegrid"]').scrollTop === 0);
    await page.getByRole('button', { name: 'Collapse Unfiled', exact: true }).click();
    await page.waitForFunction(() => document.querySelector('[role="treegrid"]').querySelectorAll('[data-testid="taste-agent-row"]').length === 1);
    await page.getByRole('button', { name: 'Expand Unfiled', exact: true }).click();
    await tree.locator('[data-testid="taste-agent-row"]').first().waitFor();
    checks.push('Folder collapse and expand retain manual control');
    await tree.hover();
    await page.mouse.wheel(0, -10000);
    await page.waitForFunction(() => document.querySelector('[role="treegrid"]').scrollTop === 0);
    const handle = page.getByRole('button', { name: 'Move Vista Agent 01', exact: true });
    await handle.focus();
    await page.keyboard.press('Enter');
    const dropLabels = [];
    for (let step = 0; step < 12; step++) {
      await page.waitForTimeout(50);
      const label = await page.evaluate(() => document.activeElement?.getAttribute('aria-label'));
      dropLabels.push(label);
      if (label === 'Drop on Vista archive') {
        await page.keyboard.press('Enter');
        await field.fill('vista agent 01');
        await page.locator('[role="row"][data-sidebar-tree-key="00000000-0000-7000-8000-000000000078"]').waitFor();
        if (await page.getByRole('row', { name: 'Unfiled', exact: true }).count() !== 0) throw new Error('Filing did not move the agent out of Unfiled');
        checks.push('Keyboard drag files the agent into the real fixture folder through the live adapter');
        return { checks, dropLabels };
      }
      await page.keyboard.press('ArrowDown');
    }
    await page.keyboard.press('Escape');
    throw new Error(`Could not reach the sibling drop target: ${JSON.stringify(dropLabels)}`);
  } finally { await context.close(); }
}
