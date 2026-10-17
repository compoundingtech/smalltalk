/** Independent browser oracle: scores actual SVG paths and label budgets, never policy outputs. */
export async function tabStripPaintedIntersection(page, base, storyId) {
  const rows = [], failures = []
  for (const viewport of [{ width: 1440, height: 900 }, { width: 1280, height: 720 }]) for (const scheme of ['dark', 'light']) {
    await page.goto('about:blank')
    await page.setViewportSize(viewport)
    await page.emulateMedia({ colorScheme: scheme })
    await page.goto(`${base}/iframe.html?id=${storyId}&viewMode=story&args=scheme:${scheme}`, { waitUntil: 'networkidle' })
    const result = await page.evaluate(async () => {
      await document.fonts.ready
      const list = document.querySelector('[data-tab-strip]')
      const stage = list.closest('[data-testid="workbench"]')
      const tabs = [...list.querySelectorAll('[role="tab"]')]
      const frames = () => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)))
      const problems = [], captures = [], totals = { widths: 0, scrollOffsets: 0, paintedIcons: 0, paintedLabels: 0, fullyHidden: 0 }
      const saved = { width: stage.style.width, flex: stage.style.flex, snap: list.style.scrollSnapType }
      const clipped = element => {
        const rect = element.getBoundingClientRect()
        const css = getComputedStyle(element)
        return { left: rect.left + element.clientLeft, right: Math.min(rect.right, rect.left + element.clientLeft + element.clientWidth), top: rect.top + element.clientTop, bottom: Math.min(rect.bottom, rect.top + element.clientTop + element.clientHeight), css }
      }
      const effectiveClip = () => {
        const clip = clipped(list)
        for (let ancestor = list.parentElement; ancestor; ancestor = ancestor.parentElement) {
          const edge = clipped(ancestor)
          if (/^(auto|scroll|hidden|clip)$/.test(edge.css.overflowX)) { clip.left = Math.max(clip.left, edge.left); clip.right = Math.min(clip.right, edge.right) }
          if (/^(auto|scroll|hidden|clip)$/.test(edge.css.overflowY)) { clip.top = Math.max(clip.top, edge.top); clip.bottom = Math.min(clip.bottom, edge.bottom) }
        }
        return clip
      }
      const painted = element => {
        for (let ancestor = element; ancestor; ancestor = ancestor.parentElement) {
          const css = getComputedStyle(ancestor)
          if (css.display === 'none' || css.visibility === 'hidden' || Number(css.opacity) === 0) return false
        }
        return true
      }
      const inspect = (width, requestedOffset, control = false) => {
        const clip = effectiveClip()
        let visibleRepresentations = 0
        for (const tab of tabs) {
          const icon = tab.querySelector('[data-tab-icon] svg')
          const label = tab.querySelector('[data-tab-label]')
          if (!tab.getAttribute('aria-label')) problems.push({ width, requestedOffset, rule: 'missing-name' })
          if (!painted(tab)) { totals.fullyHidden++; continue }
          const compact = painted(icon)
          if (!compact && !painted(label)) { problems.push({ width, requestedOffset, name: tab.getAttribute('aria-label'), rule: 'missing-representation' }); continue }
          visibleRepresentations++
          if (compact) {
            totals.paintedIcons++
            for (const shape of icon.querySelectorAll('path,rect,circle,line,polyline,polygon,ellipse')) {
              const box = shape.getBBox(), matrix = shape.getScreenCTM()
              const css = getComputedStyle(shape)
              const stroke = css.stroke === 'none' ? 0 : parseFloat(css.strokeWidth) / 2
              const points = [[box.x - stroke, box.y - stroke], [box.x + box.width + stroke, box.y - stroke], [box.x - stroke, box.y + box.height + stroke], [box.x + box.width + stroke, box.y + box.height + stroke]].map(([x, y]) => new DOMPoint(x, y).matrixTransform(matrix))
              const extent = { left: Math.min(...points.map(p => p.x)), right: Math.max(...points.map(p => p.x)), top: Math.min(...points.map(p => p.y)), bottom: Math.max(...points.map(p => p.y)) }
              const intersects = extent.right > clip.left && extent.left < clip.right && extent.bottom > clip.top && extent.top < clip.bottom
              if (intersects && (extent.left < clip.left - 0.5 || extent.right > clip.right + 0.5 || extent.top < clip.top - 0.5 || extent.bottom > clip.bottom + 0.5)) problems.push({ width, requestedOffset, offset: list.scrollLeft, name: tab.getAttribute('aria-label'), rule: 'partial-icon', extent, clip: { left: clip.left, right: clip.right, top: clip.top, bottom: clip.bottom }, control })
            }
          } else {
            totals.paintedLabels++
            const box = label.getBoundingClientRect(), css = getComputedStyle(label)
            const canvas = document.createElement('canvas'), context = canvas.getContext('2d')
            context.font = `${css.fontWeight} ${css.fontSize} ${css.fontFamily}`
            const text = Array.from(tab.getAttribute('aria-label'))
            const minimum = context.measureText(text.length > 4 ? `${text.slice(0, 4).join('')}…` : text.join('')).width
            if (box.left < clip.left - 0.5 || box.right > clip.right + 0.5 || box.top < clip.top - 0.5 || box.bottom > clip.bottom + 0.5 || box.width + 0.5 < minimum) problems.push({ width, requestedOffset, rule: 'partial-label', name: tab.getAttribute('aria-label'), box: box.toJSON(), minimum })
          }
        }
        // Snap is bypassed here, so an intermediate offset may straddle two cells. A full cell is guaranteed only
        // when the window spans cell + pitch; at a whole-cell snap anchor one cell width suffices.
        const first = tabs[0]?.getBoundingClientRect(), second = tabs[1]?.getBoundingClientRect()
        const cell = first?.width ?? 0, pitch = second && first ? second.left - first.left : cell
        const anchored = tabs.some(tab => Math.abs(tab.getBoundingClientRect().left - clip.left) < 0.5)
        if (!control && cell > 0 && clip.right - clip.left >= (anchored ? cell : cell + pitch) - 0.5 && visibleRepresentations === 0) problems.push({ width, requestedOffset, rule: 'empty-usable-strip', cell, pitch, anchored })
      }
      try {
        // Scroll snap is production-on. Temporarily bypass only snap to exercise every intermediate offset.
        list.style.scrollSnapType = 'none'
        stage.style.flex = 'none'
        for (let width = 118; width <= window.innerWidth; width++) {
          stage.style.width = `${width}px`
          await frames()
          totals.widths++
          if (Math.abs(stage.getBoundingClientRect().width - width) > 0.5) problems.push({ width, rule: 'unmeasured-width-mutation', actual: stage.getBoundingClientRect().width })
          const maximum = Math.max(0, list.scrollWidth - list.clientWidth)
          for (let requested = 0; requested <= maximum; requested++) {
            list.scrollLeft = requested
            list.dispatchEvent(new Event('scroll'))
            inspect(width, requested)
            totals.scrollOffsets++
          }
          // Fractional positions straddle cell edges that integer offsets alone can miss.
          for (const tab of tabs) {
            const box = tab.getBoundingClientRect(), clip = effectiveClip()
            for (const offset of [list.scrollLeft + box.left - clip.left - 0.25, list.scrollLeft + box.right - clip.right + 0.25]) {
              if (offset < 0 || offset > maximum) continue
              list.scrollLeft = offset
              list.dispatchEvent(new Event('scroll'))
              inspect(width, offset)
              totals.scrollOffsets++
            }
          }
          if ([118, 159, 160, 240, 320, 520, 640].includes(width)) captures.push({ width, measured: list.getBoundingClientRect().width, scrollWidth: list.scrollWidth, offset: list.scrollLeft, mode: list.dataset.tabMode })
          if (problems.length > 20) break
        }
        // A real partially painted SVG must falsify the very same path-level oracle.
        stage.style.width = '159px'
        list.scrollLeft = 0
        await frames()
        const visible = tabs.find(tab => painted(tab) && painted(tab.querySelector('[data-tab-icon] svg')))
        const icon = visible?.querySelector('[data-tab-icon] svg')
        const clip = effectiveClip(), box = icon?.getBoundingClientRect()
        const prior = problems.length
        let controlCaught = false
        if (icon && box) {
          const original = icon.style.transform
          icon.style.transform = `translateX(${clip.right - box.left - box.width / 2}px)`
          inspect(159, list.scrollLeft, true)
          controlCaught = problems.slice(prior).some(problem => problem.rule === 'partial-icon' && problem.control)
          problems.splice(prior)
          icon.style.transform = original
        }
        return { pass: problems.length === 0 && controlCaught, totals, captures, problems, controlCaught, maxCount: tabs.length, selected: tabs.find(tab => tab.getAttribute('aria-selected') === 'true')?.getAttribute('aria-label') }
      } finally { stage.style.width = saved.width; stage.style.flex = saved.flex; list.style.scrollSnapType = saved.snap; list.dispatchEvent(new Event('scroll')) }
    })
    rows.push({ viewport, scheme, ...result })
    if (!result.pass) failures.push({ viewport, scheme, ...result })
  }
  return { pass: failures.length === 0, cases: rows.length, rows, failures }
}
