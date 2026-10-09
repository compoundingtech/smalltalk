import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { geometryNumbers } from './composition-tokens.stylex'
import { FollowController } from './embrace-virtual/FollowController'
import { FollowAffordance } from './embrace-virtual/FollowAffordance'
import { returnAffordanceFocus } from './embrace-virtual/AffordancePosition'
import { captureReadingAnchor, resolveReadingAnchor, type ReadingAnchor, type SavedReadingAnchor } from './embrace-virtual/ReadingAnchor'
import { nextViewportStamp, observeViewportStamp } from './embrace-virtual/ViewportStamp'
import { ViewportPublisher } from './embrace-virtual/ViewportPublisher'

const rowSelector = '[data-item-id], [data-embrace-entry-id]'

/** Geometry and follow ownership, before the kit adds its position clock. */
export interface ViewportPosition { readonly top: number; readonly following: boolean; readonly unread: boolean; readonly anchor?: SavedReadingAnchor }
/** Portable browser-local position. The kit owns the monotonic wall-clock stamp. */
export interface ViewportState extends ViewportPosition { readonly updatedAt: number }

/** Per-surface memory, bounded to the last 100 conversations; owners drop closed keys. */
export class ViewportStore {
  private readonly states = new Map<string, ViewportState>()
  private disposed = false
  private readonly listeners = new Set<() => void>()
  private readonly viewports = new Map<() => ViewportState, string>()
  get size(): number { return this.states.size }
  get(key: string): ViewportState | undefined { return this.states.get(key) }
  save(key: string, position: ViewportPosition | ViewportState): void {
    if (this.disposed) return
    const state = 'updatedAt' in position ? position : { ...position, updatedAt: nextViewportStamp() }
    this.merge(key, state, true)
    this.notify()
  }

  /** Includes current mounted positions, even between throttled scroll notifications. */
  snapshot(): ReadonlyArray<{ readonly key: string; readonly state: ViewportState }> {
    if (this.disposed) return []
    for (const [read, key] of this.viewports) this.merge(key, read(), true)
    return Array.from(this.states, ([key, state]) => ({ key, state }))
  }

  /** Newer entries win. Mounted viewports only consume memory on a subsequent mount/activation. */
  hydrate(entries: ReadonlyArray<{ readonly key: string; readonly state: ViewportState }>): void {
    if (this.disposed) return
    let changed = false
    for (const entry of entries) changed = this.merge(entry.key, entry.state, false) || changed
    if (changed) this.notify()
  }

  subscribe = (listener: () => void) => {
    this.listeners.add(listener)
    return () => { this.listeners.delete(listener) }
  }

  /** @internal Active adapters supply geometry without subscribing the message subtree to memory. */
  trackViewport(key: string, read: () => ViewportState) {
    this.viewports.set(read, key)
    return () => { this.viewports.delete(read) }
  }

  private merge(key: string, state: ViewportState, replaceEqual: boolean) {
    const current = this.states.get(key)
    if (current !== undefined && (current.updatedAt > state.updatedAt || !replaceEqual && current.updatedAt === state.updatedAt)) return false
    observeViewportStamp(state.updatedAt)
    this.states.delete(key)
    this.states.set(key, state)
    if (this.states.size > 100) this.states.delete(this.states.keys().next().value!)
    return true
  }
  private notify() { for (const listener of this.listeners) listener() }
  retain(keys: ReadonlySet<string>): void {
    for (const key of this.states.keys()) if (!keys.has(key)) this.states.delete(key)
    for (const [read, key] of this.viewports) if (!keys.has(key)) this.viewports.delete(read)
    this.notify()
  }
  open(): void { this.disposed = false }
  /** Parent-first teardown must ignore late saves from unmounting viewports. */
  dispose(): void { this.disposed = true; this.states.clear(); this.viewports.clear(); this.listeners.clear() }
}

/** Viewports outside a store-owning surface keep memory only for their own mount. */
export const ViewportStoreContext = React.createContext<ViewportStore | undefined>(undefined)

/** Scroll and row geometry are imperative; they never invalidate the message subtree. */
class ViewportController {
  private element: HTMLDivElement | null = null
  private jumpButton: HTMLButtonElement | null = null
  private readonly publisher: ViewportPublisher
  private readonly follow = new FollowController({
    onStateChange: () => { this.dock() },
    onVisibilityChange: () => { this.dock() },
    onUserIntent: () => {
      this.pressedAnchor = undefined
      this.publisher.readerIntent()
      this.pendingState = undefined
      if (this.frame !== undefined) cancelAnimationFrame(this.frame)
      this.frame = undefined
      this.scheduleCapture()
    },
    onUserScroll: () => {
      if (this.element === null) return
      this.lastTop = this.element.scrollTop
      if (this.follow.attached) this.anchor = undefined
      else this.capture()
      this.publisher.readerScroll()
    },
    schedule: () => this.schedule(),
  })
  private get following() { return this.follow.attached }
  private announcement: HTMLSpanElement | null = null
  private anchor: ReadingAnchor | undefined
  private pendingState: ViewportState | undefined
  private lastTop = 0
  private frame: number | undefined
  private captureFrame: number | undefined
  /** Multiple pointer presses share one compensated row until their releases. */
  private readonly pressed = new Set<number>()
  private pressedAnchor: { pointerId: number; element: HTMLElement; offset: number } | undefined

  constructor(saved?: ViewportState) {
    this.publisher = new ViewportPublisher(() => this.released(true), saved)
    if (saved !== undefined && !saved.following) { this.pendingState = saved; this.follow.read() }
  }

  readonly bindStore = (store?: ViewportStore, key?: string) => { this.publisher.bind(store, key) }
  readonly setRunning = this.follow.setRunning
  readonly released = (active = false): ViewportState => ({
    top: active ? this.element?.scrollTop ?? this.lastTop : this.lastTop, following: this.following, unread: this.follow.showJump, updatedAt: this.publisher.updatedAt,
    anchor: this.anchor === undefined ? undefined : { rowId: this.anchor.rowId, text: this.anchor.text, offset: this.anchor.offset },
  })

  readonly resume = (saved?: ViewportState) => {
    this.publisher.restore(saved)
    this.anchor = undefined
    this.pressedAnchor = undefined
    this.pendingState = saved !== undefined && !saved.following ? saved : undefined
    if (this.pendingState === undefined) this.jump()
    else { this.follow.read(); this.schedule() }
  }
  readonly attachJump = (button: HTMLButtonElement | null) => {
    this.jumpButton = button
    this.dock()
  }

  /** Visibility follows reading intent and the live-edge band, never an unread-content counter. */
  private dock() {
    if (this.jumpButton !== null) {
      if (!this.follow.showJump) returnAffordanceFocus(this.jumpButton, this.element)
      this.jumpButton.hidden = !this.follow.showJump
    }
    if (this.announcement !== null) this.announcement.textContent = this.follow.showJump ? 'Reading earlier messages. Scroll to end is available.' : ''
    if (this.element !== null) this.element.dataset.followState = this.following ? 'attached' : 'detached'
  }

  readonly attachAnnouncement = (element: HTMLSpanElement | null) => {
    this.announcement = element
    this.dock()
  }

  private capture() {
    if (this.element !== null) this.anchor = captureReadingAnchor(this.element)
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
    this.lastTop = target
    if (Math.abs(target - element.scrollTop) < geometryNumbers.scrollEndTolerance) return
    element.scrollTop = target
    this.follow.markWrite(element.scrollTop)
  }

  /** Pin the pressed row, not the first visible row: insertion above must not move its action. */
  readonly preservePress = () => {
    const viewport = this.element
    const anchor = this.pressedAnchor
    if (viewport === null || anchor === undefined || !anchor.element.isConnected) return false
    this.writeTop(viewport.scrollTop + anchor.element.getBoundingClientRect().top - viewport.getBoundingClientRect().top - anchor.offset)
    // Hand history back in the compensated coordinate system; a stale offset would undo the press scroll.
    if (this.anchor?.element.isConnected) this.anchor.offset = this.anchor.element.getBoundingClientRect().top - viewport.getBoundingClientRect().top
    return true
  }

  readonly schedule = () => {
    if (this.frame !== undefined) return
    this.frame = requestAnimationFrame(() => {
      this.frame = undefined
      const element = this.element
      if (element === null) return
      // The pressed row takes precedence over the reader's history anchor.
      if (this.preservePress()) return
      if (!this.following && this.pendingState !== undefined) {
        const saved = this.pendingState
        const anchor = saved.anchor === undefined ? undefined : resolveReadingAnchor(element, saved.anchor)
        if (anchor !== undefined) {
          this.anchor = anchor
          this.writeTop(element.scrollTop + anchor.element.getBoundingClientRect().top - element.getBoundingClientRect().top - anchor.offset)
          this.pendingState = undefined
        } else {
          this.writeTop(saved.top)
          if (saved.anchor === undefined) { this.pendingState = undefined; this.capture() }
        }
      } else if (!this.following && this.captureFrame === undefined && this.anchor !== undefined) {
        const anchor = this.anchor.element.isConnected ? this.anchor : resolveReadingAnchor(element, this.anchor)
        if (anchor !== undefined) {
          this.anchor = anchor
          this.writeTop(element.scrollTop + anchor.element.getBoundingClientRect().top - element.getBoundingClientRect().top - anchor.offset)
        }
      }
      this.follow.pin(element)
    })
  }

  readonly jump = (animate = false) => {
    this.anchor = undefined
    this.pendingState = undefined
    this.pressedAnchor = undefined
    this.follow.jump(animate)
    this.dock()
    this.publisher.reattach()
  }
  readonly activate = () => { this.jump(true) }

  readonly attach = (element: HTMLDivElement | null) => {
    if (element === null) return
    this.element = element
    const detachFollow = this.follow.attach(element)
    this.dock()
    // Document-wide compensation keeps a row action under its active pointer.
    const page = element.ownerDocument
    const view = page.defaultView
    const press = (event: PointerEvent) => {
      this.pressed.add(event.pointerId)
      if (this.pressedAnchor !== undefined || !(event.target instanceof Element)) return
      const row = event.target.closest<HTMLElement>(rowSelector)
      if (row !== null && element.contains(row)) {
        this.follow.pauseMotion()
        this.pressedAnchor = { pointerId: event.pointerId, element: row, offset: row.getBoundingClientRect().top - element.getBoundingClientRect().top }
      }
    }
    const release = (event: PointerEvent) => {
      this.pressed.delete(event.pointerId)
      if (this.pressedAnchor?.pointerId === event.pointerId) this.pressedAnchor = undefined
      this.dock()
      if (this.pressed.size === 0) this.schedule()
    }
    // A release the page never sees (the window blurs, the tab hides) must not latch the dock.
    const abandon = () => {
      this.pressed.clear()
      this.pressedAnchor = undefined
      this.dock()
      this.schedule()
    }
    const hidden = () => { if (page.visibilityState === 'hidden') abandon() }
    const observer = new ResizeObserver(() => { this.preservePress(); this.schedule() })
    observer.observe(element)
    if (element.firstElementChild !== null) observer.observe(element.firstElementChild)
    page.addEventListener('pointerdown', press, true)
    page.addEventListener('pointerup', release, true)
    page.addEventListener('pointercancel', release, true)
    page.addEventListener('lostpointercapture', release, true)
    page.addEventListener('visibilitychange', hidden)
    view?.addEventListener('blur', abandon)
    this.schedule()
    return () => {
      detachFollow()
      observer.disconnect()
      if (this.frame !== undefined) cancelAnimationFrame(this.frame)
      this.frame = undefined
      if (this.captureFrame !== undefined) cancelAnimationFrame(this.captureFrame)
      this.captureFrame = undefined
      page.removeEventListener('pointerdown', press, true)
      page.removeEventListener('pointerup', release, true)
      page.removeEventListener('pointercancel', release, true)
      page.removeEventListener('lostpointercapture', release, true)
      page.removeEventListener('visibilitychange', hidden)
      view?.removeEventListener('blur', abandon)
      this.pressed.clear()
      this.publisher.detach()
      this.pressedAnchor = undefined
      this.element = null
    }
  }
}

/** Stable row identity; versions remain accepted for host compatibility. */
export interface ViewportRow { readonly id: string; readonly version?: string }

export interface EmbraceScrollViewportProps extends React.HTMLAttributes<HTMLDivElement> {
  readonly items: readonly ViewportRow[]
  readonly contentProps?: React.HTMLAttributes<HTMLDivElement>
  /** Conversation identity. In-app returns restore reading state from the enclosing ViewportStore. */
  readonly stateKey?: string
  /** Host command: a changed, defined key resumes following; an unchanged key never scrolls. */
  readonly scrollToBottomKey?: string
  /** Derived by Transcript from the running turn; running catch-up is eased, idle catch-up is instant. */
  readonly isRunning?: boolean
}

export const EmbraceScrollViewport = React.memo(function EmbraceScrollViewport({ items, children, contentProps, stateKey, scrollToBottomKey, isRunning = false, ...props }: EmbraceScrollViewportProps) {
  const store = React.useContext(ViewportStoreContext)
  const [controller] = React.useState(() => new ViewportController(stateKey === undefined ? undefined : store?.get(stateKey)))
  const previousItems = React.useRef(items)
  const previousKey = React.useRef(stateKey)
  const previousCommand = React.useRef(scrollToBottomKey)
  React.useLayoutEffect(() => {
    controller.setRunning(isRunning)
    if (stateKey !== previousKey.current) {
      controller.bindStore()
      if (previousKey.current !== undefined) store?.save(previousKey.current, controller.released())
      controller.bindStore(store, stateKey)
      previousKey.current = stateKey
      controller.resume(stateKey === undefined ? undefined : store?.get(stateKey))
    } else {
      controller.bindStore(store, stateKey)
      if (scrollToBottomKey !== undefined && scrollToBottomKey !== previousCommand.current) controller.jump()
      else if (previousItems.current !== items) controller.schedule()
    }
    // A switch adopts its own command key; returning to a reading thread is not an own send.
    previousCommand.current = scrollToBottomKey
    previousItems.current = items
  }, [controller, items, store, stateKey, scrollToBottomKey, isRunning])
  // Every layout commit can insert above a pressed row, including runtime adoption without new items.
  React.useLayoutEffect(() => { controller.preservePress() })
  React.useLayoutEffect(() => () => {
    if (previousKey.current !== undefined) store?.save(previousKey.current, controller.released())
  }, [controller, store])
  return <div {...stylex.props(styles.frame)}>
    <div {...props} tabIndex={props.tabIndex ?? -1} style={{ ...props.style, overflowAnchor: 'none' }} ref={controller.attach}><div {...contentProps}>{children}</div></div>
    <FollowAffordance buttonRef={controller.attachJump} onPress={controller.activate} />
    <span ref={controller.attachAnnouncement} role="status" aria-live="polite" {...stylex.props(styles.announcement)} />
  </div>
})

const styles = stylex.create({
  frame: { position: 'relative', display: 'flex', flexDirection: 'column', flex: '1 1 0', minHeight: 0, minWidth: 0 },
  announcement: { position: 'absolute', width: 1, height: 1, overflow: 'hidden', clipPath: 'inset(50%)', whiteSpace: 'nowrap' },
})
