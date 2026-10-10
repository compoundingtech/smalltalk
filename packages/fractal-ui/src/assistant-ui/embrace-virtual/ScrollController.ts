import type { ListLayout } from 'react-aria-components'
import { geometryNumbers } from '../composition-tokens.stylex'
import { returnAffordanceFocus } from './AffordancePosition'

/** The source transcript persists a mode or a stable entry, never a numeric bottom. */
export type ScrollAnchor =
  | { readonly _tag: 'Following' }
  | {
      readonly _tag: 'Reading'
      readonly entry: string | null
      readonly offset: number
      readonly scrollTop: number
    }

type Row = { readonly id: string }
type ReadingPosition = { readonly key: string; readonly offset: number }

const navigationKeys: Readonly<Record<string, true>> = { PageUp: true, PageDown: true, Home: true, End: true, ArrowUp: true, ArrowDown: true, ' ': true }
const USER_SCROLL_WINDOW_MS = 250

/**
 * Follows the app's long-session virtual conversation. The kit has one renderer per
 * entry, so it needs no block/outline-to-entry mapping.
 * RAC measurement can move scrollTop without user input; only user scrolls leave follow mode.
 */
export class ScrollController {
  private element: HTMLDivElement | null = null
  private jumpButton: HTMLButtonElement | null = null
  private programmaticTop: number | undefined
  private following: boolean
  private restoring: boolean
  private userInputAt = -Infinity
  private userScrolling = false
  private anchor: ReadingPosition | undefined
  private pendingAnchor: ReadingPosition | undefined
  private restoreFrame: number | undefined
  private firstKey: string | undefined
  private rows: readonly Row[] = []
  private rowKeys = new Set<string>()

  constructor(
    private readonly options: {
      readonly layout: ListLayout<unknown>
      readonly initial: ScrollAnchor
      readonly save: (anchor: ScrollAnchor) => void
    },
  ) {
    this.following = options.initial._tag === 'Following'
    this.restoring = options.initial._tag === 'Reading'
    if (options.initial._tag === 'Reading' && options.initial.entry !== null) {
      this.pendingAnchor = { key: options.initial.entry, offset: options.initial.offset }
    }
  }

  readonly attachJump = (button: HTMLButtonElement | null) => {
    this.jumpButton = button
    this.dock()
  }

  private dock() {
    const element = this.element
    if (element === null) return
    element.dataset.followState = this.following ? 'attached' : 'detached'
    if (this.jumpButton !== null) {
      this.jumpButton.hidden = this.following || element.scrollHeight - element.clientHeight - element.scrollTop <= geometryNumbers.followAffordanceBand
    }
  }

  private atEnd() {
    const element = this.element
    return element !== null && element.scrollHeight - element.clientHeight - element.scrollTop <= geometryNumbers.scrollEndTolerance
  }

  private writeTop(top: number) {
    const element = this.element
    if (element === null) return
    const target = Math.max(0, Math.min(top, element.scrollHeight - element.clientHeight))
    if (Math.abs(target - element.scrollTop) < geometryNumbers.scrollEndTolerance) return
    element.scrollTop = target
    this.programmaticTop = element.scrollTop
  }

  /** Mounted geometry is authoritative; public layout geometry recovers an offscreen stable key. */
  private compensateAnchor() {
    const element = this.element
    const anchor = this.anchor
    if (element === null || anchor === undefined || !this.rowKeys.has(anchor.key)) return
    const target = element.querySelector<HTMLElement>(`[data-embrace-entry-id="${CSS.escape(anchor.key)}"]`)
    if (target !== null) this.writeTop(element.scrollTop + target.getBoundingClientRect().top - element.getBoundingClientRect().top - anchor.offset)
    else {
      const info = this.options.layout.getLayoutInfo(anchor.key)
      if (info !== null) this.writeTop(info.rect.y - anchor.offset)
    }
    this.savePosition()
  }

  readonly jump = () => {
    if (this.jumpButton !== null) returnAffordanceFocus(this.jumpButton, this.element)
    this.userScrolling = false
    this.userInputAt = -Infinity
    this.anchor = undefined
    this.following = true
    this.restoring = false
    this.pendingAnchor = undefined
    if (this.restoreFrame !== undefined) cancelAnimationFrame(this.restoreFrame)
    this.restoreFrame = undefined
    if (this.element !== null) this.writeTop(this.element.scrollHeight)
    this.dock()
    this.options.save({ _tag: 'Following' })
  }

  /** React 19 cleans up the ref when the actual RAC scroll element unmounts. */
  readonly attach = (element: HTMLDivElement | null) => {
    if (element === null) return
    this.element = element
    const onUserInput = (event: Event) => {
      if (event instanceof KeyboardEvent && (navigationKeys[event.key] !== true || (event.target instanceof HTMLElement && event.target.closest('input,textarea,[contenteditable="true"]')))) return
      this.userInputAt = performance.now()
      this.userScrolling = true
      this.programmaticTop = undefined
      this.pendingAnchor = undefined
      this.restoring = false
      if (this.restoreFrame !== undefined) cancelAnimationFrame(this.restoreFrame)
      this.restoreFrame = undefined
    }
    const onScroll = () => {
      if (this.atEnd()) {
        this.following = true
        this.anchor = undefined
        this.pendingAnchor = undefined
        this.restoring = false
        this.dock()
        this.savePosition()
        return
      }
      if (this.programmaticTop !== undefined && Math.abs(element.scrollTop - this.programmaticTop) < geometryNumbers.scrollEndTolerance) {
        this.programmaticTop = undefined
        return
      }
      if (this.restoring || this.pendingAnchor !== undefined) return
      if (!this.userScrolling && performance.now() - this.userInputAt > USER_SCROLL_WINDOW_MS) {
        // RAC can move an attached lane without resizing it (for example, an ack
        // re-key). Correct the delivered displacement rather than waiting for RO.
        if (this.following) this.writeTop(element.scrollHeight)
        this.dock()
        return
      }
      this.captureAnchor()
      this.following = false
      this.dock()
      this.savePosition()
    }
    const onScrollEnd = () => {
      if (this.following && !this.userScrolling && !this.restoring && this.pendingAnchor === undefined) this.writeTop(element.scrollHeight)
      if (this.userScrolling || (!this.restoring && this.pendingAnchor === undefined && this.atEnd())) {
        this.following = this.atEnd()
        if (this.following) {
          this.anchor = undefined
          this.pendingAnchor = undefined
        } else this.captureAnchor()
        this.dock()
        this.savePosition()
      }
      this.userScrolling = false
      this.userInputAt = -Infinity
    }
    const observer = new ResizeObserver(() => {
      // Resize has already laid out this frame. Correct before paint, with guarded
      // writes so RAC's next measurement delivery cannot create a resize loop.
      if (this.restoreFrame !== undefined) cancelAnimationFrame(this.restoreFrame)
      this.restoreFrame = undefined
      if (this.pendingAnchor !== undefined || this.restoring) this.restore()
      else if (this.following) this.writeTop(element.scrollHeight)
      else this.compensateAnchor()
      this.dock()
    })
    observer.observe(element)
    const sizer = element.firstElementChild
    if (sizer !== null) observer.observe(sizer)
    // RAC may reposition mounted rows before its estimated sizer changes. Observe
    // the entries as well so ordinary row expansion/shrink settles in that frame.
    const observedRows = new Set<HTMLElement>()
    const observeRows = () => {
      for (const row of observedRows) {
        if (!element.contains(row)) { observer.unobserve(row); observedRows.delete(row) }
      }
      for (const row of element.querySelectorAll<HTMLElement>('[data-embrace-entry-id]')) {
        if (!observedRows.has(row)) { observedRows.add(row); observer.observe(row) }
      }
      // RAC commits absolute row positions separately from size measurement.
      // Those layout commits must preserve the same anchor before their paint.
      if (!this.following) {
        if (this.pendingAnchor !== undefined || this.restoring) this.restore()
        else this.compensateAnchor()
        this.dock()
      }
    }
    const mutations = new MutationObserver(observeRows)
    mutations.observe(element, { childList: true, attributes: true, attributeFilter: ['style'], subtree: true })
    observeRows()
    const inputs = ['wheel', 'touchmove', 'keydown', 'pointerdown'] as const
    for (const type of inputs) element.addEventListener(type, onUserInput, { passive: true })
    element.addEventListener('scroll', onScroll, { passive: true })
    element.addEventListener('scrollend', onScrollEnd, { passive: true })
    return () => {
      observer.disconnect()
      mutations.disconnect()
      if (this.restoreFrame !== undefined) cancelAnimationFrame(this.restoreFrame)
      this.restoreFrame = undefined
      for (const type of inputs) element.removeEventListener(type, onUserInput)
      element.removeEventListener('scroll', onScroll)
      element.removeEventListener('scrollend', onScrollEnd)
      if (!this.restoring) this.savePosition()
      this.element = null
    }
  }

  private savePosition() {
    this.options.save(
      this.following
        ? { _tag: 'Following' }
        : {
            _tag: 'Reading',
            entry: this.anchor?.key ?? null,
            offset: this.anchor?.offset ?? 0,
            scrollTop: this.element?.scrollTop ?? 0,
          },
    )
  }

  private captureAnchor() {
    const element = this.element
    if (element === null) return
    const top = element.getBoundingClientRect().top
    let nearest: HTMLElement | undefined
    let nearestTop = Infinity
    for (const candidate of element.querySelectorAll<HTMLElement>('[data-embrace-entry-id]')) {
      const rect = candidate.getBoundingClientRect()
      if (rect.bottom > top && rect.top < nearestTop) {
        nearest = candidate
        nearestTop = rect.top
      }
    }
    const key = nearest?.dataset['embraceEntryId']
    if (key !== undefined) this.anchor = { key, offset: nearestTop - top }
  }

  private scheduleRestore() {
    if (this.restoreFrame !== undefined) return
    this.restoreFrame = requestAnimationFrame(() => {
      this.restoreFrame = undefined
      this.restore()
    })
  }

  private restore() {
    const anchor = this.pendingAnchor
    const element = this.element
    if (element === null) return
    const key = anchor !== undefined && this.rowKeys.has(anchor.key) ? anchor.key : undefined
    const info = key === undefined ? null : this.options.layout.getLayoutInfo(key)
    if (
      this.restoring &&
      key === undefined &&
      this.firstKey !== undefined &&
      this.options.layout.getLayoutInfo(this.firstKey) !== null &&
      element.scrollHeight > 0
    ) {
      // An absent saved entry can fall back only once collection geometry exists.
      this.writeTop(this.options.initial._tag === 'Reading' ? this.options.initial.scrollTop : 0)
      this.captureAnchor()
      this.pendingAnchor = this.anchor
      this.restoring = false
      return
    }
    // RAC publishes its collection after the parent commit. Keep the stable anchor
    // through estimated and measured layouts instead of consuming it prematurely.
    if (
      anchor !== undefined &&
      key !== undefined &&
      info !== null &&
      element.scrollHeight >= info.rect.y + Math.min(info.rect.height, element.clientHeight)
    ) {
      this.writeTop(info.rect.y - anchor.offset)
      const target = element.querySelector<HTMLElement>(
        `[data-embrace-entry-id="${CSS.escape(key)}"]`,
      )
      if (target !== null) {
        this.writeTop(element.scrollTop +
          target.getBoundingClientRect().top - element.getBoundingClientRect().top - anchor.offset)
        this.anchor = { key, offset: anchor.offset }
        this.pendingAnchor = undefined
        this.restoring = false
        this.savePosition()
      }
    }
    if (this.restoring) this.scheduleRestore()
  }

  /** Call after a commit that changes the generic entry collection. */
  afterRowsChange(rows: readonly Row[]) {
    const previous = this.rows
    this.rows = rows
    this.firstKey = rows[0]?.id
    if (previous !== rows) {
      this.rowKeys.clear()
      for (const row of rows) this.rowKeys.add(row.id)
    }
    if (this.restoring && this.element !== null && rows.length > 0) this.restore()
    if (this.following) {
      if (this.element !== null) this.writeTop(this.element.scrollHeight)
      this.dock()
      return
    }
    if (this.anchor !== undefined && previous !== rows) {
      // Every changed collection can resize above the reader, not just a prepend.
      this.pendingAnchor = this.anchor
      this.restore()
      if (this.pendingAnchor !== undefined) this.scheduleRestore()
    } else if (this.pendingAnchor !== undefined) {
      this.restore()
    }
    this.dock()
  }
}

/**
 * Workshop-local storage; independent of the app's state registry. A missing, unreadable or
 * malformed value resumes following instead of throwing out of render.
 */
export function readScrollAnchor(key: string): ScrollAnchor {
  if (typeof window === 'undefined') return { _tag: 'Following' }
  let value: unknown
  try {
    const raw = window.localStorage.getItem(`fractal-ui.embrace:${key}:conversation.anchor`)
    if (raw === null) return { _tag: 'Following' }
    value = JSON.parse(raw)
  } catch {
    return { _tag: 'Following' }
  }
  if (
    typeof value === 'object' && value !== null && '_tag' in value &&
    value._tag === 'Reading' &&
    'entry' in value && (value.entry === null || typeof value.entry === 'string') &&
    'offset' in value && typeof value.offset === 'number' && Number.isFinite(value.offset) &&
    'scrollTop' in value && typeof value.scrollTop === 'number' && Number.isFinite(value.scrollTop)
  ) return { _tag: 'Reading', entry: value.entry, offset: value.offset, scrollTop: value.scrollTop }
  return { _tag: 'Following' }
}

export function saveScrollAnchor(key: string, anchor: ScrollAnchor) {
  if (typeof window !== 'undefined') {
    window.localStorage.setItem(`fractal-ui.embrace:${key}:conversation.anchor`, JSON.stringify(anchor))
  }
}
