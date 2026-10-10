import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { geometryNumbers } from './composition-tokens.stylex'
import { returnAffordanceFocus } from './embrace-virtual/AffordancePosition'
import { FollowAffordance } from './embrace-virtual/FollowAffordance'

const rowSelector = '[data-item-id], [data-embrace-entry-id]'
const navigationKeys: Readonly<Record<string, true>> = { PageUp: true, PageDown: true, Home: true, End: true, ArrowUp: true, ArrowDown: true, ' ': true }
const readerScrollWindowMs = 250

/** A bounded lane owns scrolling; a misconfigured host may leave it to an ancestor or the page. */
function pressScrollOwner(lane: HTMLElement): HTMLElement {
  const view = lane.ownerDocument.defaultView
  for (let candidate: HTMLElement | null = lane; candidate !== null; candidate = candidate.parentElement) {
    if (candidate.scrollHeight > candidate.clientHeight && /^(auto|scroll|overlay)$/.test(view?.getComputedStyle(candidate).overflowY ?? '')) return candidate
  }
  const documentOwner = lane.ownerDocument.scrollingElement
  return documentOwner instanceof HTMLElement && documentOwner.scrollHeight > documentOwner.clientHeight ? documentOwner : lane
}

/** Scroll state a conversation keeps while it stays on a surface. */
export interface ViewportState { readonly top: number; readonly following: boolean; readonly unread: boolean }

/** Per-surface memory, one entry per conversation. Owners drop closed keys and dispose on unmount. */
export class ViewportStore {
  private readonly states = new Map<string, ViewportState>()
  private disposed = false
  get size(): number { return this.states.size }
  get(key: string): ViewportState | undefined { return this.states.get(key) }
  save(key: string, state: ViewportState): void { if (!this.disposed) this.states.set(key, state) }
  retain(keys: ReadonlySet<string>): void { for (const key of this.states.keys()) if (!keys.has(key)) this.states.delete(key) }
  open(): void { this.disposed = false }
  /** Parent-first teardown must ignore late saves from unmounting viewports. */
  dispose(): void { this.disposed = true; this.states.clear() }
}

/** Viewports outside a store-owning surface keep memory only for their own mount. */
export const ViewportStoreContext = React.createContext<ViewportStore | undefined>(undefined)

/** Scroll and row geometry are imperative; they never invalidate the message subtree. */
class ViewportController {
  private element: HTMLDivElement | null = null
  private jumpButton: HTMLButtonElement | null = null
  private following = true
  private anchor: { element: HTMLElement; offset: number } | undefined
  private frame: number | undefined
  private captureFrame: number | undefined
  private unread = false
  private lastTop = 0
  private programmaticTop: number | undefined
  private readerInputAt = -Infinity
  private readerGesture = false
  private restored: ViewportState | undefined
  /** A restored line whose content is settling; the first reader scroll clears it. */
  private pendingTop: number | undefined
  /** Pointers down somewhere on the page; the dock keeps its layout until all are released. */
  private readonly pressed = new Set<number>()
  private readonly readerPointers = new Set<number>()
  private pressedAnchor: { pointerId: number; element: HTMLElement; scrollOwner: HTMLElement; top: number } | undefined
  private warnedScrollOwner = false

  constructor(saved?: ViewportState) {
    if (saved !== undefined && !saved.following) {
      this.restored = saved
      this.unread = saved.unread
    }
  }

  readonly attachJump = (button: HTMLButtonElement | null) => {
    this.jumpButton = button
    this.dock()
  }

  /** The pill offers the end whenever the reader is away from it; an active press keeps its layout until release. */
  private dock() {
    const element = this.element
    if (element === null) return
    element.dataset.followState = this.following ? 'attached' : 'detached'
    if (this.jumpButton === null || this.pressed.size > 0) return
    this.jumpButton.hidden = this.following || element.scrollHeight - element.clientHeight - element.scrollTop <= geometryNumbers.scrollEndTolerance
  }

  readonly released = (): ViewportState => ({ top: this.lastTop, following: this.following, unread: this.unread })

  /** Swaps a reused viewport to another conversation without carrying its unread mark across. */
  readonly resume = (saved?: ViewportState) => {
    this.readerInputAt = -Infinity
    this.readerGesture = false
    this.unread = saved !== undefined && !saved.following && saved.unread
    this.dock()
    if (saved !== undefined && !saved.following) {
      this.following = false
      this.anchor = undefined
      this.pendingTop = saved.top
      this.writeTop(saved.top)
      this.scheduleCapture()
      this.schedule()
    } else {
      this.following = true
      this.anchor = undefined
      if (this.element !== null) this.writeTop(this.element.scrollHeight)
      this.schedule()
    }
  }

  private capture() {
    const element = this.element
    if (element === null) return
    const viewport = element.getBoundingClientRect()
    const top = viewport.top
    const hit = element.ownerDocument.elementFromPoint(viewport.left + viewport.width / 2, top + geometryNumbers.scrollEndTolerance)?.closest<HTMLElement>(rowSelector)
    if (hit !== undefined && hit !== null && element.contains(hit)) {
      this.anchor = { element: hit, offset: hit.getBoundingClientRect().top - top }
      return
    }
    for (const row of element.querySelectorAll<HTMLElement>(rowSelector)) {
      const bounds = row.getBoundingClientRect()
      if (bounds.bottom > top) {
        this.anchor = { element: row, offset: bounds.top - top }
        return
      }
    }
  }
  private scheduleCapture() {
    if (this.captureFrame !== undefined) return
    this.captureFrame = requestAnimationFrame(() => {
      this.captureFrame = undefined
      if (!this.following) this.capture()
    })
  }


  private writeTop(top: number) {
    const element = this.element
    if (element === null) return
    const target = Math.max(0, Math.min(top, element.scrollHeight - element.clientHeight))
    if (Math.abs(target - element.scrollTop) < geometryNumbers.scrollEndTolerance) return
    element.scrollTop = target
    this.programmaticTop = element.scrollTop
    this.lastTop = element.scrollTop
  }

  /** Pin the pressed row, not the first visible row: insertion above must not move its action. */
  readonly preservePress = () => {
    const viewport = this.element
    const anchor = this.pressedAnchor
    if (viewport === null || anchor === undefined || !anchor.element.isConnected) return false
    const delta = anchor.element.getBoundingClientRect().top - anchor.top
    if (anchor.scrollOwner === viewport) this.writeTop(viewport.scrollTop + delta)
    else anchor.scrollOwner.scrollTop += delta
    // Hand history back in the compensated coordinate system; a stale offset would undo the press scroll.
    if (this.anchor?.element.isConnected) this.anchor.offset = this.anchor.element.getBoundingClientRect().top - viewport.getBoundingClientRect().top
    return true
  }

  /** One layout pass: the pressed row, then the end or the reader's anchor. */
  private settle() {
    const element = this.element
    if (element === null) return
    // The pressed row takes precedence over the reader's history anchor.
    if (this.preservePress()) return
    if (this.following) this.writeTop(element.scrollHeight)
    else if (this.pendingTop !== undefined) {
      this.writeTop(this.pendingTop)
      if (Math.abs(element.scrollTop - Math.max(0, Math.min(this.pendingTop, element.scrollHeight - element.clientHeight))) < geometryNumbers.scrollEndTolerance) {
        this.pendingTop = undefined
        this.scheduleCapture()
      }
    } else if (this.captureFrame === undefined && this.anchor?.element.isConnected) {
      this.writeTop(element.scrollTop + this.anchor.element.getBoundingClientRect().top - element.getBoundingClientRect().top - this.anchor.offset)
    }
    if (!this.following) this.dock()
  }

  readonly schedule = () => {
    if (this.frame !== undefined) return
    this.frame = requestAnimationFrame(() => {
      this.frame = undefined
      this.settle()
    })
  }

  readonly jump = () => {
    if (this.jumpButton !== null) returnAffordanceFocus(this.jumpButton, this.element)
    this.readerInputAt = -Infinity
    this.readerGesture = false
    this.following = true
    this.unread = false
    this.anchor = undefined
    this.pendingTop = undefined
    this.dock()
    this.schedule()
  }

  readonly scrollTo = (top: number) => {
    this.following = false
    this.readerInputAt = -Infinity
    this.readerGesture = false
    this.pendingTop = undefined
    this.pressedAnchor = undefined
    this.anchor = undefined
    this.writeTop(top)
    if (this.element !== null) this.lastTop = this.element.scrollTop
    this.dock()
    this.scheduleCapture()
  }

  readonly changed = () => {
    if (!this.following) {
      this.unread = true
      this.dock()
    }
    this.schedule()
  }

  /** Backfill is a layout change, not reader input. Keep end or the visible row. */
  readonly preserveLayout = (change: () => void) => {
    if (!this.following) this.capture()
    change()
    const element = this.element
    if (element === null) return
    if (!this.preservePress()) {
      if (this.following) this.writeTop(element.scrollHeight)
      else if (this.anchor?.element.isConnected) {
        this.writeTop(element.scrollTop + this.anchor.element.getBoundingClientRect().top - element.getBoundingClientRect().top - this.anchor.offset)
      }
    }
    this.schedule()
  }

  readonly attach = (element: HTMLDivElement | null) => {
    if (element === null) return
    this.element = element
    const manual = (event: Event) => {
      if (event instanceof KeyboardEvent && (navigationKeys[event.key] !== true || (event.target instanceof HTMLElement && event.target.closest('input,textarea,[contenteditable="true"]')))) return
      if (event.type === 'wheel' || event.type === 'touchmove' || event.type === 'keydown') {
        this.readerInputAt = performance.now()
        this.readerGesture = true
      }
      if (event.type === 'wheel' || event.type === 'touchmove' || event.type === 'keydown') this.pressedAnchor = undefined
      this.following = false
      this.dock()
      this.programmaticTop = undefined
      this.pendingTop = undefined
      this.scheduleCapture()
      if (this.frame !== undefined) cancelAnimationFrame(this.frame)
      this.frame = undefined
    }
    const scroll = () => {
      if (element.scrollHeight - element.clientHeight - element.scrollTop <= geometryNumbers.scrollEndTolerance) {
        this.following = true
        this.unread = false
        this.anchor = undefined
        this.pendingTop = undefined
        this.programmaticTop = undefined
        this.lastTop = element.scrollTop
        this.dock()
        return
      }
      if (this.programmaticTop !== undefined && Math.abs(element.scrollTop - this.programmaticTop) < geometryNumbers.scrollEndTolerance) {
        this.programmaticTop = undefined
        return
      }
      if (Math.abs(element.scrollTop - this.lastTop) < geometryNumbers.scrollEndTolerance) return
      if (this.pendingTop !== undefined) {
        this.writeTop(this.pendingTop)
        return
      }
      const readerIntent = this.readerGesture || this.readerPointers.size > 0 || performance.now() - this.readerInputAt <= readerScrollWindowMs
      if (!readerIntent) {
        if (this.following) this.schedule()
        else {
          this.lastTop = element.scrollTop
          this.dock()
          this.scheduleCapture()
        }
        return
      }
      this.pressedAnchor = undefined
      this.lastTop = element.scrollTop
      this.following = false
      this.dock()
      this.scheduleCapture()
    }
    const scrollend = () => {
      this.readerGesture = false
      this.readerInputAt = -Infinity
    }
    // Document-wide, so presses that start anywhere (a row action included) defer the reveal.
    const page = element.ownerDocument
    const view = page.defaultView
    const press = (event: PointerEvent) => {
      this.pressed.add(event.pointerId)
      if (!(event.target instanceof Element)) return
      if (element.contains(event.target)) this.readerPointers.add(event.pointerId)
      if (this.pressedAnchor !== undefined) return
      const row = event.target.closest<HTMLElement>(rowSelector)
      if (row !== null && element.contains(row)) {
        const scrollOwner = pressScrollOwner(element)
        if (scrollOwner !== element && !this.warnedScrollOwner && process.env.NODE_ENV !== 'production') {
          this.warnedScrollOwner = true
          console.warn('EmbraceScrollViewport: the lane is not its own scroll container. Bound its height so following and history anchoring stay lane-local; held row actions use the actual scroll owner.')
        }
        this.pressedAnchor = { pointerId: event.pointerId, element: row, scrollOwner, top: row.getBoundingClientRect().top }
      }
    }
    const release = (event: PointerEvent) => {
      this.pressed.delete(event.pointerId)
      this.readerPointers.delete(event.pointerId)
      if (this.pressedAnchor?.pointerId === event.pointerId) this.pressedAnchor = undefined
      this.dock()
    }
    // A release the page never sees (the window blurs, the tab hides) must not latch the dock.
    const abandon = () => {
      scrollend()
      this.pressed.clear()
      this.readerPointers.clear()
      this.pressedAnchor = undefined
      this.dock()
    }
    const hidden = () => { if (page.visibilityState === 'hidden') abandon() }
    // Resize delivery has already laid out the new geometry: settle before paint and drop the pending frame.
    const observer = new ResizeObserver(() => {
      if (this.frame !== undefined) cancelAnimationFrame(this.frame)
      this.frame = undefined
      this.settle()
    })
    observer.observe(element)
    if (element.firstElementChild !== null) observer.observe(element.firstElementChild)
    element.addEventListener('wheel', manual, { passive: true })
    element.addEventListener('touchmove', manual, { passive: true })
    element.addEventListener('keydown', manual)
    element.addEventListener('pointerdown', manual, { passive: true })
    element.addEventListener('focusin', manual)
    element.addEventListener('scroll', scroll, { passive: true })
    element.addEventListener('scrollend', scrollend, { passive: true })
    page.addEventListener('pointerdown', press, true)
    page.addEventListener('pointerup', release, true)
    page.addEventListener('pointercancel', release, true)
    page.addEventListener('lostpointercapture', release, true)
    page.addEventListener('visibilitychange', hidden)
    view?.addEventListener('blur', abandon)
    if (this.restored !== undefined) {
      this.following = false
      this.pendingTop = this.restored.top
      this.writeTop(this.restored.top)
      this.lastTop = this.restored.top
      this.scheduleCapture()
      this.restored = undefined
    }
    this.schedule()
    return () => {
      observer.disconnect()
      if (this.frame !== undefined) cancelAnimationFrame(this.frame)
      this.frame = undefined
      if (this.captureFrame !== undefined) cancelAnimationFrame(this.captureFrame)
      this.captureFrame = undefined
      element.removeEventListener('wheel', manual)
      element.removeEventListener('touchmove', manual)
      element.removeEventListener('keydown', manual)
      element.removeEventListener('pointerdown', manual)
      element.removeEventListener('focusin', manual)
      element.removeEventListener('scroll', scroll)
      element.removeEventListener('scrollend', scrollend)
      page.removeEventListener('pointerdown', press, true)
      page.removeEventListener('pointerup', release, true)
      page.removeEventListener('pointercancel', release, true)
      page.removeEventListener('lostpointercapture', release, true)
      page.removeEventListener('visibilitychange', hidden)
      view?.removeEventListener('blur', abandon)
      this.pressed.clear()
      this.readerPointers.clear()
      scrollend()
      this.pressedAnchor = undefined
      this.element = null
    }
  }
}

/** `version` is the row's rendered content; without one, the row object itself is its version. */
export interface ViewportRow { readonly id: string; readonly version?: string }

/** A republished snapshot with the same rows is not news: compare ids and content versions, not array identity. */
function sameRows(previous: readonly ViewportRow[], next: readonly ViewportRow[]) {
  if (previous.length !== next.length) return false
  return next.every((row, index) => {
    const before = previous[index]!
    return before.id === row.id && (row.version === undefined ? before === row : before.version === row.version)
  })
}

export interface EmbraceScrollViewportHandle {
  /** Commit a synchronous layout change without changing the reader's follow mode. */
  readonly preserveLayout: (change: () => void) => void
  readonly scrollTo: (top: number) => void
}

export interface EmbraceScrollViewportProps extends React.HTMLAttributes<HTMLDivElement> {
  readonly ref?: React.Ref<EmbraceScrollViewportHandle>
  readonly items: readonly ViewportRow[]
  readonly contentProps?: React.HTMLAttributes<HTMLDivElement>
  /** Each key keeps its own scroll state across viewport mounts. */
  readonly stateKey?: string
  /** Host command: a changed, defined key resumes following and clears unread; an unchanged key never scrolls. */
  readonly scrollToBottomKey?: string
}

export const EmbraceScrollViewport = React.memo(function EmbraceScrollViewport({ ref: viewportRef, items, children, contentProps, stateKey, scrollToBottomKey, ...props }: EmbraceScrollViewportProps) {
  const store = React.useContext(ViewportStoreContext)
  const [controller] = React.useState(() => new ViewportController(stateKey === undefined ? undefined : store?.get(stateKey)))
  React.useImperativeHandle(viewportRef, () => controller, [controller])
  const previousItems = React.useRef(items)
  const previousKey = React.useRef(stateKey)
  const previousCommand = React.useRef(scrollToBottomKey)
  React.useLayoutEffect(() => {
    if (stateKey !== previousKey.current) {
      if (previousKey.current !== undefined) store?.save(previousKey.current, controller.released())
      previousKey.current = stateKey
      controller.resume(stateKey === undefined ? undefined : store?.get(stateKey))
    } else if (scrollToBottomKey !== undefined && scrollToBottomKey !== previousCommand.current) {
      // Following persists, so rows that commit after the command (the pending send) stay in view.
      controller.jump()
    } else if (previousItems.current !== items) {
      // Metadata-only snapshots still reach geometry; only new or changed rows count as unread.
      if (sameRows(previousItems.current, items)) controller.schedule()
      else controller.changed()
    }
    // The first render is not a command, and a conversation switch adopts its key without scrolling.
    previousCommand.current = scrollToBottomKey
    previousItems.current = items
  }, [controller, items, store, stateKey, scrollToBottomKey])
  // Every layout commit can insert above a pressed row, including runtime adoption without new items.
  React.useLayoutEffect(() => { controller.preservePress() })
  // Mutation-phase saves precede the owning surface's layout effect that removes closed keys.
  React.useLayoutEffect(() => () => {
    if (previousKey.current !== undefined) store?.save(previousKey.current, controller.released())
  }, [controller, store])
  return <div {...stylex.props(styles.frame)}>
    <div {...props} style={{ ...props.style, overflowAnchor: 'none' }} ref={controller.attach}><div {...contentProps}>{children}</div></div>
    <FollowAffordance buttonRef={controller.attachJump} onPress={controller.jump} />
  </div>
})

const styles = stylex.create({
  frame: { position: 'relative', display: 'flex', flexDirection: 'column', flex: '1 1 0', minHeight: 0, minWidth: 0 },
})
