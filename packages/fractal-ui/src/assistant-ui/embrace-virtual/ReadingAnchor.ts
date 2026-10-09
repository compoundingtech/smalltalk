export interface SavedReadingAnchor { readonly rowId: string; readonly text?: string; readonly offset: number }
export interface ReadingAnchor extends SavedReadingAnchor { readonly element: HTMLElement; offset: number }
const rowSelector = '[data-item-id], [data-embrace-entry-id]'
const textSelector = 'p,li,pre,h1,h2,h3,[data-follow-anchor]'

/** Anchor a visible text block, not its enclosing turn: disclosures inside the turn may grow above it. */
export function captureReadingAnchor(viewport: HTMLDivElement): ReadingAnchor | undefined {
  const bounds = viewport.getBoundingClientRect()
  const hit = viewport.ownerDocument.elementFromPoint(bounds.left + bounds.width / 2, bounds.top + 1)?.closest<HTMLElement>(textSelector)
  let element = hit !== undefined && hit !== null && viewport.contains(hit) ? hit : undefined
  if (element === undefined) {
    for (const candidate of viewport.querySelectorAll<HTMLElement>(textSelector)) {
      if (candidate.getBoundingClientRect().bottom > bounds.top) { element = candidate; break }
    }
  }
  if (element === undefined) {
    for (const candidate of viewport.querySelectorAll<HTMLElement>(rowSelector)) {
      if (candidate.getBoundingClientRect().bottom > bounds.top) { element = candidate; break }
    }
  }
  const row = element?.closest<HTMLElement>('[data-embrace-entry-id]') ?? element?.closest<HTMLElement>('[data-item-id]')
  const rowId = row?.dataset['embraceEntryId'] ?? row?.dataset['itemId']
  if (element === undefined || rowId === undefined) return
  return { element, rowId, text: element === row ? undefined : element.textContent?.trim().slice(0, 80), offset: element.getBoundingClientRect().top - bounds.top }
}

/** Stable row identity plus text survives a parked/remounted conversation and insertions inside a turn. */
export function resolveReadingAnchor(viewport: HTMLDivElement, saved: SavedReadingAnchor): ReadingAnchor | undefined {
  const escaped = CSS.escape(saved.rowId)
  const row = viewport.querySelector<HTMLElement>(`[data-item-id="${escaped}"],[data-embrace-entry-id="${escaped}"]`)
  if (row === null) return
  let element: HTMLElement | undefined = saved.text === undefined ? row : undefined
  if (saved.text !== undefined) {
    for (const candidate of row.querySelectorAll<HTMLElement>(textSelector)) {
      if (candidate.textContent?.trim().slice(0, 80) === saved.text) { element = candidate; break }
    }
  }
  return element === undefined ? undefined : { ...saved, element }
}
