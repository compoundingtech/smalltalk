import {
  layoutTabStrip,
  layoutTabStripViewport,
  TAB_STRIP_POLICY,
  type TabStripLayout,
  type TabStripMode,
  type TabStripRect,
} from './tab-strip-layout.ts'

// Standalone pure TypeScript suite: no DOM, React, runner or Node type dependency.
const assert = (condition: boolean, message: string): void => {
  if (!condition) throw new Error(message)
}
const equal = (actual: unknown, expected: unknown, message: string): void => {
  assert(JSON.stringify(actual) === JSON.stringify(expected), `${message}: ${JSON.stringify(actual)} != ${JSON.stringify(expected)}`)
}
const near = (actual: number, expected: number, message: string): void => {
  assert(Math.abs(actual - expected) <= Number.EPSILON * Math.max(1, Math.abs(actual), Math.abs(expected)) * 4, message)
}
let tests = 0
const test = (name: string, body: () => void): void => {
  body()
  tests += 1
  console.log(`ok ${tests} - ${name}`)
}
const rejects = (body: () => void, reason: string): void => {
  let rejected = false
  try { body() } catch (error) {
    assert(error instanceof Error && error.message === reason, 'control failed for an unexpected reason')
    rejected = true
  }
  assert(rejected, `control did not falsify ${reason}`)
}
const metrics = { iconCellWidth: 24, maxCellWidth: 128, gap: 4, labelPadding: 8 }
const items = (count: number, minimumLabelWidth = 40) =>
  Array.from({ length: count }, (_, index) => ({ key: `resource-${index}`, minimumLabelWidth }))
const layout = (availableWidth: number, previousMode?: TabStripMode, count = 4, minimumLabelWidth = 40): TabStripLayout =>
  layoutTabStrip({ availableWidth, previousMode, items: items(count, minimumLabelWidth), metrics })

test('immutable measured-strip policy and initial exact/fractional boundary', () => {
  equal(TAB_STRIP_POLICY, { measuredWidthFloor: 360, restoreHeadroom: 16 }, 'product thresholds')
  assert(Object.isFrozen(TAB_STRIP_POLICY), 'policy must be immutable')
  for (const width of [0, 118, 159, 160, 359.75, 360, 360.25, 375.75, 376, 520]) {
    equal(layout(width).mode, width >= 360 ? 'labels' : 'icons', `initial ${width}`)
  }
  equal(layout(360).collapseBelow, 360, 'measured floor')
  equal(layout(360).expandAt, 376, 'restore headroom')
})

test('both hysteresis histories and repeated sub-band oscillation', () => {
  for (const width of [359.75, 360, 360.25, 375.75, 376, 376.25]) {
    equal(layout(width, 'labels').mode, width >= 360 ? 'labels' : 'icons', `label history ${width}`)
    equal(layout(width, 'icons').mode, width >= 376 ? 'labels' : 'icons', `icon history ${width}`)
  }
  let mode: TabStripMode = 'icons'
  for (let cycle = 0; cycle < 32; cycle += 1) {
    mode = layout(360.25, mode).mode
    equal(mode, 'icons', 'deadband must retain icons')
    mode = layout(375.75, mode).mode
    equal(mode, 'icons', 'oscillation must not restore')
  }
  mode = layout(376, mode).mode
  equal(mode, 'labels', 'exact restoration')
  equal(layout(359.75, mode).mode, 'icons', 'collapse is immediate')
})

test('largest minimum label, padding and gaps determine content-fit floor', () => {
  // Eight equal cells need 56px each and seven 4px gaps: 476px.
  equal(layout(476, undefined, 8).collapseBelow, 476, 'independent content budget')
  equal(layout(476, undefined, 8).expandAt, 492, 'content restore budget')
  for (const width of [475.75, 476, 476.25, 491.75, 492]) {
    equal(layout(width, 'labels', 8).mode, width >= 476 ? 'labels' : 'icons', 'fit collapse')
    equal(layout(width, 'icons', 8).mode, width >= 492 ? 'labels' : 'icons', 'fit restore')
  }
  const varied = items(4)
  varied[3] = { key: 'longest', minimumLabelWidth: 100 }
  const result = layoutTabStrip({ availableWidth: 475.75, items: varied, metrics, previousMode: 'labels' })
  equal(result.collapseBelow, 476, 'equal cells require largest minimum in every track')
  equal(result.mode, 'icons', 'hard fit violation ignores prior label mode')
})

test('cell cap prevents impossible label restoration, including changed titles', () => {
  const atCap = layout(2000, undefined, 1, 112)
  equal(atCap.mode, 'labels', 'minimum equals cap')
  equal(atCap.cellWidth, 128, 'maximum token cap')
  for (const previousMode of [undefined, 'labels', 'icons'] as const) {
    const impossible = layout(2000, previousMode, 1, 112.25)
    equal(impossible.mode, 'icons', 'minimum above cap never fits')
    assert(impossible.collapseBelow === Infinity && impossible.expandAt === Infinity, 'impossible fit thresholds')
    equal(impossible.cellWidth, atCap.cellWidth, 'title changes never reallocate tracks')
  }
})

test('empty, zero, negative and token-dependent allocation', () => {
  equal(layout(0), { mode: 'icons', cellWidth: 24, contentWidth: 108, collapseBelow: 360, expandAt: 376 }, 'zero mount reserves tracks')
  equal(layout(-1), layout(0), 'negative available space behaves as zero')
  equal(layout(0, undefined, 0), { mode: 'icons', cellWidth: 0, contentWidth: 0, collapseBelow: 360, expandAt: 376 }, 'empty zero mount')
  equal(layout(360, undefined, 0).mode, 'labels', 'empty measured mount follows initial threshold')
  equal(layout(20, undefined, 1).cellWidth, 24, 'one item keeps compact floor')
  equal(layout(90, undefined, 1).cellWidth, 90, 'one item uses available budget')
  const custom = layoutTabStrip({ availableWidth: 100, items: items(8), metrics: { iconCellWidth: 32, maxCellWidth: 144, gap: 2, labelPadding: 6 } })
  equal(custom.cellWidth, 32, 'floor comes from actual token metrics')
  equal(custom.contentWidth, 270, 'floor overflow includes all gaps')
})

test('all integer widths and up to eight resources obey independent equal-cell formulas', () => {
  for (let count = 1; count <= 8; count += 1) {
    for (let width = 0; width <= 1200; width += 1) {
      const expectedCell = Math.max(24, Math.min(128, (width - 4 * (count - 1)) / count))
      const expectedCollapse = Math.max(360, count * 56 + 4 * (count - 1))
      for (const previousMode of [undefined, 'labels', 'icons'] as const) {
        const result = layout(width, previousMode, count)
        near(result.cellWidth, expectedCell, 'equal cell floor/cap/budget')
        near(result.contentWidth, count * expectedCell + 4 * (count - 1), 'independent content width')
        equal(result.collapseBelow, expectedCollapse, 'independent label boundary')
        equal(result.mode, width >= expectedCollapse + (previousMode === 'icons' ? 16 : 0) ? 'labels' : 'icons', 'independent mode oracle')
      }
    }
  }
})

test('mode, identity and title changes do not alter allocation or mutate inputs', () => {
  const input = Object.freeze({ availableWidth: 368, items: Object.freeze(items(4).map((item) => Object.freeze(item))), metrics: Object.freeze({ ...metrics }) })
  const labelled = layoutTabStrip({ ...input, previousMode: 'labels' })
  const compact = layoutTabStrip({ ...input, previousMode: 'icons' })
  equal(labelled.mode, 'labels', 'label deadband')
  equal(compact.mode, 'icons', 'icon deadband')
  equal([labelled.cellWidth, labelled.contentWidth], [compact.cellWidth, compact.contentWidth], 'mode-independent tracks')
  const renamed = layoutTabStrip({ ...input, items: input.items.map((item) => ({ key: `${item.key}-other-kind`, minimumLabelWidth: 120 })) })
  equal([renamed.cellWidth, renamed.contentWidth], [labelled.cellWidth, labelled.contentWidth], 'keys/kinds/title minima cannot allocate')
  equal(layoutTabStrip(input), layoutTabStrip(input), 'deterministic layout')
})

type Cell = TabStripRect & { key: string }
const cellRects = (count: number, cellWidth: number, gap: number, scrollOffset: number, left = 0): Cell[] =>
  Array.from({ length: count }, (_, index) => ({ key: `resource-${index}`, left: left + index * (cellWidth + gap) - scrollOffset, right: left + index * (cellWidth + gap) + cellWidth - scrollOffset, top: 10, bottom: 34 }))
const fullyInside = (cell: Cell, clip: TabStripRect, shift = 0): boolean => {
  if (cell.right <= cell.left || cell.bottom <= cell.top || clip.right <= clip.left || clip.bottom <= clip.top) return false
  // Independent signed edge-distance oracle: at most one arithmetic ULP.
  const edges: [number, number, number][] = [
    [cell.left + shift, clip.left, 1],
    [cell.right + shift, clip.right, -1],
    [cell.top, clip.top, 1],
    [cell.bottom, clip.bottom, -1],
  ]
  return edges.every(([edge, boundary, direction]) =>
    (edge - boundary) * direction >= -Number.EPSILON * Math.max(1, Math.abs(edge), Math.abs(boundary)))
}

test('both clipping axes, effective ancestor intersection and empty rectangles', () => {
  const clip = { left: 10, right: 110, top: 10, bottom: 34 }
  const cells: Cell[] = [
    { key: 'whole', left: 10, right: 34, top: 10, bottom: 34 },
    { key: 'left', left: 9.75, right: 33.75, top: 10, bottom: 34 },
    { key: 'right', left: 86.25, right: 110.25, top: 10, bottom: 34 },
    { key: 'top', left: 40, right: 64, top: 9.75, bottom: 33.75 },
    { key: 'bottom', left: 40, right: 64, top: 10.25, bottom: 34.25 },
    { key: 'zero-width', left: 40, right: 40, top: 10, bottom: 34 },
    { key: 'zero-height', left: 40, right: 64, top: 10, bottom: 10 },
    { key: 'offscreen', left: 200, right: 224, top: 10, bottom: 34 },
  ]
  equal(layoutTabStripViewport({ cells, clip, scrollOffset: 20 }).paintedKeys, ['whole'], 'full rectangles only')
  // Adapter-supplied intersection of strip [0,120] and ancestor [10,110].
  equal(layoutTabStripViewport({ cells, clip: { ...clip, right: 10 }, scrollOffset: 20, revealKey: 'whole' }), { scrollOffset: 20, paintedKeys: [] }, 'zero clip')
  equal(layoutTabStripViewport({ cells, clip: { ...clip, bottom: 9 }, scrollOffset: 20 }).paintedKeys, [], 'disjoint vertical clip')
  equal(layoutTabStripViewport({ cells: [], clip, scrollOffset: 20, revealKey: 'missing' }), { scrollOffset: 20, paintedKeys: [] }, 'empty collection')
})

test('first/last reveal shifts viewport-relative cells by old minus new offset', () => {
  const clip = { left: 50, right: 150, top: 10, bottom: 34 }
  const cells = cellRects(8, 24, 4, 20, 50)
  const first = layoutTabStripViewport({ cells, clip, scrollOffset: 20, revealKey: 'resource-0' })
  equal(first.scrollOffset, 0, 'first cell aligns left')
  assert(first.paintedKeys.includes('resource-0'), 'first paints after reveal')
  const last = layoutTabStripViewport({ cells, clip, scrollOffset: 20, revealKey: 'resource-7' })
  equal(last.scrollOffset, 120, 'last cell aligns right')
  assert(last.paintedKeys.includes('resource-7'), 'last paints after reveal')
  equal(layoutTabStripViewport({ cells, clip, scrollOffset: 20, revealKey: 'resource-2' }).scrollOffset, 20, 'already whole target does not scroll')
  equal(layoutTabStripViewport({ cells, clip, scrollOffset: 20, revealKey: 'missing' }).scrollOffset, 20, 'absent target does not scroll')
  for (const revealKey of ['resource-0', 'resource-7']) {
    const result = layoutTabStripViewport({ cells, clip: { ...clip, right: 70 }, scrollOffset: 20, revealKey })
    equal(result.scrollOffset, 20, 'oversized target cannot reveal whole')
    equal(result.paintedKeys, [], 'viewport below one cell paints nothing')
  }
})

for (let count = 1; count <= 8; count += 1) {
  test(`exhaustive widths/scroll offsets and selected ends with ${count} resources`, () => {
    for (let width = 0; width <= 640; width += 1) {
      const allocation = layout(width, undefined, count)
      const clip = { left: 0, right: width, top: 10, bottom: 34 }
      const maxOffset = Math.max(0, allocation.contentWidth - width)
      for (let offsetQuarter = 0; offsetQuarter <= Math.ceil(maxOffset * 4); offsetQuarter += 1) {
        const offset = offsetQuarter / 4
        const cells = cellRects(count, allocation.cellWidth, 4, offset)
        const actual = layoutTabStripViewport({ cells, clip, scrollOffset: offset })
        equal(actual.scrollOffset, offset, 'native scrolling cannot alter selection or request another scroll')
        equal(actual.paintedKeys, cells.filter((cell) => fullyInside(cell, clip)).map((cell) => cell.key), `independent whole-cell oracle at width ${width}, offset ${offset}`)
        for (const revealKey of ['resource-0', `resource-${count - 1}`]) {
          const revealed = layoutTabStripViewport({ cells, clip, scrollOffset: offset, revealKey })
          if (width >= allocation.cellWidth) assert(revealed.paintedKeys.includes(revealKey), 'selected first/last must paint whole after reveal')
          else equal(revealed.paintedKeys, [], 'too-small viewport cannot paint a partial target')
        }
      }
    }
  })
}

test('159/160 eight-item fractional width and edge offsets never admit real partial paint', () => {
  for (const width of [118, 158.75, 159, 159.25, 159.75, 160, 160.25, 359.75, 360, 360.25]) {
    const allocation = layout(width, undefined, 8)
    const clip = { left: 0.25, right: width + 0.25, top: 10, bottom: 34 }
    for (const offset of [0, 0.25, 0.5, 0.75, 1, 27.75, 28, 28.25, allocation.contentWidth - width]) {
      const cells = cellRects(8, allocation.cellWidth, 4, offset, clip.left)
      equal(layoutTabStripViewport({ cells, clip, scrollOffset: offset }).paintedKeys, cells.filter((cell) => fullyInside(cell, clip)).map((cell) => cell.key), 'fractional independent containment')
      for (const revealKey of ['resource-0', 'resource-7']) {
        assert(layoutTabStripViewport({ cells, clip, scrollOffset: offset, revealKey }).paintedKeys.includes(revealKey), 'fractional selected endpoint reveal')
      }
    }
  }
  const clip = { left: 0, right: 24, top: 0, bottom: 24 }
  for (const delta of [1e-9, 0.001, 0.25, 0.5]) {
    const cells = [{ key: 'partial', left: -delta, right: 24 - delta, top: 0, bottom: 24 }]
    equal(layoutTabStripViewport({ cells, clip, scrollOffset: 0 }).paintedKeys, [], 'no CSS-pixel geometric tolerance')
  }
  for (const origin of [0.1, 0.2, 0.3]) {
    for (const offset of [17.1, 17.2, 17.3]) {
      const fractionalCells = cellRects(8, 24.1, 4.1, offset, origin)
      for (const revealKey of ['resource-0', 'resource-7']) {
        assert(layoutTabStripViewport({ cells: fractionalCells, clip: { left: origin, right: origin + 159.1, top: 10, bottom: 34 }, scrollOffset: offset, revealKey }).paintedKeys.includes(revealKey), 'nonbinary fractional arithmetic reveal')
      }
    }
  }
})

test('paired controls falsify threshold, fit and partial-intersection policies', () => {
  const thresholdOracle = (mode: TabStripMode): void => assert(mode === 'icons', 'restore hysteresis ignored')
  thresholdOracle(layout(368, 'icons').mode)
  rejects(() => thresholdOracle(layout(368).mode), 'restore hysteresis ignored')
  const fitOracle = (mode: TabStripMode): void => assert(mode === 'icons', 'content-fit floor ignored')
  fitOracle(layout(400, 'labels', 8).mode)
  rejects(() => fitOracle(400 >= 360 ? 'labels' : 'icons'), 'content-fit floor ignored')
  const clip = { left: 0, right: 159, top: 10, bottom: 34 }
  const cells = cellRects(8, 24, 4, 1)
  const paintOracle = (keys: string[]): void => assert(keys.every((key) => cells.some((cell) => cell.key === key && fullyInside(cell, clip))), 'partial edge painted')
  paintOracle(layoutTabStripViewport({ cells, clip, scrollOffset: 1 }).paintedKeys)
  rejects(() => paintOracle(cells.filter((cell) => cell.right > clip.left && cell.left < clip.right).map((cell) => cell.key)), 'partial edge painted')
})

console.log(`Passed ${tests} pure tab-strip policy tests`)
