import type { ListLayout } from 'react-aria-components'

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

const FOLLOW_SLACK = 48
const USER_SCROLL_WINDOW_MS = 250

/**
 * Follows the app's long-session virtual conversation. The kit has one renderer per
 * entry, so it needs no block/outline-to-entry mapping.
 * RAC measurement can move scrollTop without user input; only user scrolls leave follow mode.
 */
export class ScrollController {
  private element: HTMLDivElement | null = null
  private following: boolean
  private restoring: boolean
  private userInputAt = 0
  private userScrolling = false
  private anchor: ReadingPosition | undefined
  private pendingAnchor: ReadingPosition | undefined
  private restoreFrame: number | undefined
  private firstKey: string | undefined
  private rows: readonly Row[] = []
  private rowKeys = new Set<string>()
  private geometry = { contentHeight: 0, viewportHeight: 0 }

  constructor(
    private readonly options: {
      readonly layout: ListLayout<unknown>
      readonly initial: ScrollAnchor
      readonly save: (anchor: ScrollAnchor) => void
      readonly setUnread: (unread: boolean) => void
    },
  ) {
    this.following = options.initial._tag === 'Following'
    this.restoring = options.initial._tag === 'Reading'
    if (options.initial._tag === 'Reading' && options.initial.entry !== null) {
      this.pendingAnchor = { key: options.initial.entry, offset: options.initial.offset }
    }
  }

  readonly jump = () => {
    this.following = true
    this.restoring = false
    this.pendingAnchor = undefined
    if (this.restoreFrame !== undefined) cancelAnimationFrame(this.restoreFrame)
    this.restoreFrame = undefined
    if (this.element !== null) this.element.scrollTop = this.element.scrollHeight
    this.options.setUnread(false)
    this.options.save({ _tag: 'Following' })
  }

  /** React 19 cleans up the ref when the actual RAC scroll element unmounts. */
  readonly attach = (element: HTMLDivElement | null) => {
    if (element === null) return
    this.element = element
    const onUserInput = () => {
      this.userInputAt = performance.now()
      this.userScrolling = true
      this.pendingAnchor = undefined
      this.restoring = false
      if (this.restoreFrame !== undefined) cancelAnimationFrame(this.restoreFrame)
      this.restoreFrame = undefined
    }
    const onScroll = () => {
      const resized =
        element.scrollHeight !== this.geometry.contentHeight ||
        element.clientHeight !== this.geometry.viewportHeight
      if (resized && performance.now() - this.userInputAt > USER_SCROLL_WINDOW_MS) return
      if (this.restoring || this.pendingAnchor !== undefined) return
      if (!this.userScrolling && performance.now() - this.userInputAt > USER_SCROLL_WINDOW_MS)
        return
      this.captureAnchor()
      this.following =
        element.scrollHeight - element.scrollTop - element.clientHeight <= FOLLOW_SLACK
      if (this.following) this.options.setUnread(false)
      this.savePosition()
    }
    const onScrollEnd = () => {
      if (this.userScrolling && !this.following) this.captureAnchor()
      this.userScrolling = false
    }
    let resizeFrame: number | undefined
    const observer = new ResizeObserver(() => {
      // Scroll writes inside RO can synchronously remount RAC rows and resize its
      // sizer again. Coalesce them into the next frame rather than a resize loop.
      if (resizeFrame !== undefined) return
      resizeFrame = requestAnimationFrame(() => {
        resizeFrame = undefined
        if (this.pendingAnchor !== undefined || this.restoring) this.restore()
        else if (this.following) element.scrollTop = element.scrollHeight
        this.geometry = { contentHeight: element.scrollHeight, viewportHeight: element.clientHeight }
      })
    })
    observer.observe(element)
    const sizer = element.firstElementChild
    if (sizer !== null) observer.observe(sizer)
    const inputs = ['wheel', 'touchmove', 'keydown', 'pointerdown'] as const
    for (const type of inputs) element.addEventListener(type, onUserInput, { passive: true })
    element.addEventListener('scroll', onScroll, { passive: true })
    element.addEventListener('scrollend', onScrollEnd, { passive: true })
    return () => {
      observer.disconnect()
      if (resizeFrame !== undefined) cancelAnimationFrame(resizeFrame)
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
      element.scrollTop = this.options.initial._tag === 'Reading' ? this.options.initial.scrollTop : 0
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
      element.scrollTop = info.rect.y - anchor.offset
      const target = element.querySelector<HTMLElement>(
        `[data-embrace-entry-id="${CSS.escape(key)}"]`,
      )
      if (target !== null) {
        element.scrollTop +=
          target.getBoundingClientRect().top - element.getBoundingClientRect().top - anchor.offset
        this.anchor = { key, offset: anchor.offset }
        this.pendingAnchor = this.anchor
        this.restoring = false
        this.savePosition()
      }
    }
    if (this.restoring) this.scheduleRestore()
  }

  /** Call after a commit that changes the generic entry collection. */
  afterRowsChange(rows: readonly Row[]) {
    const previous = this.rows
    const previousFirst = this.firstKey
    this.rows = rows
    this.firstKey = rows[0]?.id
    if (previous !== rows) {
      this.rowKeys.clear()
      for (const row of rows) this.rowKeys.add(row.id)
    }
    if (this.restoring && this.element !== null && rows.length > 0) this.restore()
    if (this.following) {
      this.options.setUnread(false)
      if (this.element !== null) this.element.scrollTop = this.element.scrollHeight
      return
    }
    if (
      this.anchor !== undefined &&
      previousFirst !== undefined &&
      this.firstKey !== previousFirst &&
      this.rowKeys.has(previousFirst)
    ) {
      // The collection rebuild follows this commit. ResizeObserver restores before
      // paint when the sizer grows; the tracked animation frame is the fallback.
      this.pendingAnchor = this.anchor
      this.scheduleRestore()
    } else if (this.pendingAnchor !== undefined) {
      this.restore()
    }
    if (previous.length === 0 || previous === rows || this.restoring) return
    const start = rows.findIndex((row) => row.id === previous[0]?.id)
    if (
      start < 0 ||
      rows.length - start !== previous.length ||
      previous.some((row, index) => row !== rows[start + index])
    ) this.options.setUnread(true)
  }
}

/** Workshop-local storage; independent of the app's state registry. */
export function readScrollAnchor(key: string): ScrollAnchor {
  if (typeof window === 'undefined') return { _tag: 'Following' }
  const raw = window.localStorage.getItem(`fractal-ui.embrace:${key}:conversation.anchor`)
  if (raw === null) return { _tag: 'Following' }
  const value: unknown = JSON.parse(raw)
  if (typeof value === 'object' && value !== null && '_tag' in value) {
    if (value._tag === 'Following') return { _tag: 'Following' }
    if (
      value._tag === 'Reading' &&
      'entry' in value && (value.entry === null || typeof value.entry === 'string') &&
      'offset' in value && typeof value.offset === 'number' && Number.isFinite(value.offset) &&
      'scrollTop' in value && typeof value.scrollTop === 'number' && Number.isFinite(value.scrollTop)
    ) return { _tag: 'Reading', entry: value.entry, offset: value.offset, scrollTop: value.scrollTop }
  }
  throw new TypeError('Invalid persisted transcript scroll anchor')
}

export function saveScrollAnchor(key: string, anchor: ScrollAnchor) {
  if (typeof window !== 'undefined') {
    window.localStorage.setItem(`fractal-ui.embrace:${key}:conversation.anchor`, JSON.stringify(anchor))
  }
}
