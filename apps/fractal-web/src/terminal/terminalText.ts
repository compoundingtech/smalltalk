/** DOM rows stay the authority for visible text, including redaction and truncation markers. */
export const terminalText = ({
  viewport,
  selection,
}: {
  readonly viewport: HTMLElement
  readonly selection?: Selection | null | undefined
}): string => {
  const selected =
    selection !== undefined && selection !== null && !selection.isCollapsed
      ? selection.getRangeAt(0)
      : undefined
  const rows = viewport.querySelectorAll<HTMLElement>('[data-terminal-row]')
  let text = ''
  let previous: HTMLElement | undefined
  for (const row of rows) {
    if (selected !== undefined && !selected.intersectsNode(row)) continue
    const clipped = selected?.cloneRange()
    if (clipped !== undefined) {
      if (clipped.comparePoint(row, 0) === 0) clipped.setStart(row, 0)
      if (clipped.comparePoint(row, row.childNodes.length) === 0)
        clipped.setEnd(row, row.childNodes.length)
    }
    if (previous !== undefined && row.dataset.wrapped !== 'true') text += '\n'
    text +=
      clipped === undefined ? (row.textContent ?? '') : (clipped.cloneContents().textContent ?? '')
    previous = row
  }
  return text
}

/** Case-insensitive literal search includes matches split across style runs and soft wraps. */
export const terminalMatches = ({
  viewport,
  query,
}: {
  readonly viewport: HTMLElement
  readonly query: string
}): readonly Range[] => {
  if (query.length === 0) return []
  const nodes: Array<{ readonly node: Text; readonly start: number; readonly end: number }> = []
  let text = ''
  for (const row of viewport.querySelectorAll<HTMLElement>('[data-terminal-row]')) {
    if (text.length !== 0 && row.dataset.wrapped !== 'true') text += '\n'
    const walker = document.createTreeWalker(row, NodeFilter.SHOW_TEXT)
    let node = walker.nextNode()
    while (node !== null) {
      if (node instanceof Text) {
        const start = text.length
        text += node.data
        nodes.push({ node, start, end: text.length })
      }
      node = walker.nextNode()
    }
  }
  const pattern = new RegExp(query.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'giu')
  const matches: Range[] = []
  let firstIndex = 0
  let lastIndex = 0
  for (const match of text.matchAll(pattern)) {
    const start = match.index
    const end = start + match[0].length
    while (nodes[firstIndex] !== undefined && nodes[firstIndex]!.end <= start) firstIndex += 1
    lastIndex = Math.max(firstIndex, lastIndex)
    while (nodes[lastIndex] !== undefined && nodes[lastIndex]!.end < end) lastIndex += 1
    const first = nodes[firstIndex]
    const last = nodes[lastIndex]
    if (first === undefined || last === undefined || first.start > start || last.end < end) continue
    const range = document.createRange()
    range.setStart(first.node, start - first.start)
    range.setEnd(last.node, end - last.start)
    matches.push(range)
  }
  return matches
}
