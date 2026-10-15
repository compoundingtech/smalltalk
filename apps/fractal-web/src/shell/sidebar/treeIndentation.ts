export type TreeIndentationMeasurement = {
  readonly leading: readonly number[]
  readonly deltas: readonly number[]
  readonly textOffsets: readonly number[]
  readonly rightEdges: readonly number[]
  readonly highlightEdges: readonly number[]
  readonly timeStarts: readonly number[]
  readonly countStarts: readonly number[]
}

/** Measure production glyphs and text, not padding declarations or story-only markers. */
export const measureTreeIndentation = (surface: HTMLElement): TreeIndentationMeasurement => {
  const required = (selector: string): HTMLElement => {
    const element = surface.querySelector<HTMLElement>(selector)
    if (element === null) throw new Error(`Tree indentation element missing: ${selector}`)
    return element
  }
  const leading = [
    '[data-key="product"] button[slot="chevron"] svg',
    '[data-key="interface"] button[slot="chevron"] svg',
    '[data-key="agent/builder"] [data-row-column="status"] svg',
    '[data-wf-agent-ref="agent/builder/subagent/review"] [data-row-column="status"] svg',
    '[data-wf-agent-ref="agent/builder/subagent/review/subagent/check"] [data-row-column="status"] svg',
    '[data-wf-agent-ref="agent/builder/subagent/review/subagent/check/subagent/detail"] [data-row-column="status"] svg',
  ].map(selector => required(selector).getBoundingClientRect().left)
  const rows = [...surface.querySelectorAll<HTMLElement>('[data-testid="taste-agent-row"]')]
  return {
    leading,
    deltas: leading.slice(1).map((x, index) => x - leading[index]!),
    textOffsets: rows.map(row => row.querySelector('[data-row-column="title-text"]')!.getBoundingClientRect().left - row.querySelector('[data-row-column="subtitle"]')!.getBoundingClientRect().left),
    rightEdges: rows.map(row => row.querySelector('[data-row-column="time"]')!.getBoundingClientRect().right),
    highlightEdges: rows.map(row => row.getBoundingClientRect().left),
    timeStarts: rows.map(row => row.querySelector('[data-row-column="time"]')!.getBoundingClientRect().left),
    countStarts: rows.map(row => row.querySelector('[data-row-signal="X-unread"]')!.getBoundingClientRect().left),
  }
}

export const assertTreeIndentation = (measurement: TreeIndentationMeasurement): void => {
  for (const [index, delta] of measurement.deltas.entries()) {
    if (Math.abs(delta - 16) > 0.5) throw new Error(`Tree indent depth ${index + 1}: expected 16px, received ${delta}px`)
  }
  for (const offset of measurement.textOffsets) {
    if (Math.abs(offset) > 0.5) throw new Error(`Agent title/metadata start differs by ${offset}px`)
  }
  for (const edge of measurement.rightEdges) {
    if (Math.abs(edge - measurement.rightEdges[0]!) > 0.5) throw new Error(`Right column moved by ${edge - measurement.rightEdges[0]!}px`)
  }
  for (const edge of measurement.highlightEdges) {
    if (Math.abs(edge - measurement.highlightEdges[0]!) > 0.5) throw new Error(`Full-width highlight moved by ${edge - measurement.highlightEdges[0]!}px`)
  }
  for (const column of [measurement.timeStarts, measurement.countStarts]) {
    for (const start of column) {
      if (Math.abs(start - column[0]!) > 0.5) throw new Error(`Right column start moved by ${start - column[0]!}px`)
    }
  }
}
