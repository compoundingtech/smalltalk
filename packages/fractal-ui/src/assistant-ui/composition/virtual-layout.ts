import type { Key, ListLayout } from 'react-aria-components'

/** Reveal a collection key, leaving room for a sticky inset. Returns false until RAC has its key. */
export function scrollToLayoutKey(layout: ListLayout<unknown>, key: Key, viewport: HTMLElement, topInset = 0): boolean {
  const info = layout.getLayoutInfo(key)
  if (info === null) return false
  viewport.scrollTop = info.rect.y - topInset
  return true
}
