export type TabStripMode = 'labels' | 'icons'

/** Product thresholds use measured strip space, never the enclosing pane width. */
export const TAB_STRIP_POLICY = Object.freeze({ measuredWidthFloor: 360, restoreHeadroom: 16 })

export interface TabStripLayoutInput {
  availableWidth: number
  items: readonly { key: string; minimumLabelWidth: number }[]
  metrics: {
    iconCellWidth: number
    maxCellWidth: number
    gap: number
    /** Horizontal label inset on each side of a cell. */
    labelPadding: number
  }
  previousMode?: TabStripMode
}

export interface TabStripLayout {
  mode: TabStripMode
  cellWidth: number
  contentWidth: number
  collapseBelow: number
  expandAt: number
}

/**
 * Equal tracks are independent of representation, titles and selection. Metrics
 * are resolved CSS-pixel token geometry (the adapter supplies the 24px hitbox).
 * A minimum label that exceeds the cell cap can never restore labelled mode.
 */
export const layoutTabStrip = ({
  availableWidth,
  items,
  metrics,
  previousMode,
}: TabStripLayoutInput): TabStripLayout => {
  const width = Math.max(0, availableWidth)
  const floor = Math.max(0, metrics.iconCellWidth)
  // Preserve the hitbox even if a transient measurement reports a smaller cap.
  const cap = Math.max(floor, metrics.maxCellWidth)
  const gap = Math.max(0, metrics.gap)
  const padding = Math.max(0, metrics.labelPadding)
  const count = items.length
  const gaps = Math.max(0, count - 1) * gap
  let minimumCell = floor
  for (const item of items) {
    minimumCell = Math.max(minimumCell, Math.max(0, item.minimumLabelWidth) + 2 * padding)
  }
  const requiredLabelBudget = minimumCell > cap ? Infinity : count * minimumCell + gaps
  const collapseBelow = Math.max(TAB_STRIP_POLICY.measuredWidthFloor, requiredLabelBudget)
  const expandAt = collapseBelow + TAB_STRIP_POLICY.restoreHeadroom
  const cellWidth = count === 0 ? 0 : Math.max(floor, Math.min(cap, (width - gaps) / count))
  const contentWidth = count === 0 ? 0 : count * cellWidth + gaps
  const threshold = previousMode === 'icons' ? expandAt : collapseBelow
  const mode = width > 0 && width >= threshold && minimumCell <= cap ? 'labels' : 'icons'
  return { mode, cellWidth, contentWidth, collapseBelow, expandAt }
}

export interface TabStripRect {
  left: number
  right: number
  top: number
  bottom: number
}

export interface TabStripViewportInput {
  /** Actual viewport-relative whole-cell rectangles, in collection order. */
  cells: readonly (TabStripRect & { key: string })[]
  /** Effective intersection of the strip and all clipping ancestors. */
  clip: TabStripRect
  scrollOffset: number
  revealKey?: string
}

export interface TabStripViewportLayout {
  scrollOffset: number
  paintedKeys: string[]
}

// Only absorb arithmetic roundoff, not CSS-pixel measurement/paint tolerance.
const atLeast = (value: number, boundary: number): boolean =>
  value >= boundary || boundary - value <= Number.EPSILON * Math.max(1, Math.abs(value), Math.abs(boundary))
const atMost = (value: number, boundary: number): boolean =>
  value <= boundary || value - boundary <= Number.EPSILON * Math.max(1, Math.abs(value), Math.abs(boundary))

/**
 * Reveal a whole target with the smallest horizontal scroll adjustment, then
 * decide paint against the new viewport coordinates. No partial cell paints,
 * including at arbitrary native scroll positions or vertical ancestor clips.
 * An oversized/vertically clipped target cannot be revealed horizontally and
 * remains unpainted. Native scroll range clamping belongs to the DOM adapter.
 */
export const layoutTabStripViewport = ({
  cells,
  clip,
  scrollOffset,
  revealKey,
}: TabStripViewportInput): TabStripViewportLayout => {
  const paintedKeys: string[] = []
  if (clip.right <= clip.left || clip.bottom <= clip.top) return { scrollOffset, paintedKeys }

  let nextOffset = scrollOffset
  if (revealKey !== undefined) {
    const target = cells.find((cell) => cell.key === revealKey)
    if (target !== undefined && target.right > target.left && target.right - target.left <= clip.right - clip.left) {
      if (target.left < clip.left) nextOffset += target.left - clip.left
      else if (target.right > clip.right) nextOffset += target.right - clip.right
    }
  }
  const shift = scrollOffset - nextOffset
  // Move the clip instead of subtracting nearly equal numbers in each cell;
  // this is equivalent to shifting cells by old-new, without edge cancellation.
  const shiftedLeft = clip.left - shift
  const shiftedRight = clip.right - shift
  for (const cell of cells) {
    if (
      cell.right > cell.left && cell.bottom > cell.top &&
      atLeast(cell.left, shiftedLeft) && atMost(cell.right, shiftedRight) &&
      atLeast(cell.top, clip.top) && atMost(cell.bottom, clip.bottom)
    ) paintedKeys.push(cell.key)
  }
  return { scrollOffset: nextOffset, paintedKeys }
}
