import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button } from 'react-aria-components'
import { surfaceVars, textVars, borderVars, radiusVars, spaceVars, typeVars, geometryNumbers } from './composition-tokens.stylex'
import { FollowController } from './embrace-virtual/FollowController'

const rowSelector = '[data-item-id], [data-embrace-entry-id]'

/** @deprecated Conversation opens now always attach to the live edge; saved positions are not restored. */
export interface ViewportState { readonly top: number; readonly following: boolean; readonly unread: boolean }

/** @deprecated Retained for host compatibility. Viewports no longer read or write parked positions. */
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

/** @deprecated Retained for host compatibility; conversation opens always start at the bottom. */
export const ViewportStoreContext = React.createContext<ViewportStore | undefined>(undefined)

/** Scroll and row geometry are imperative; they never invalidate the message subtree. */
class ViewportController {
  private element: HTMLDivElement | null = null
  private jumpButton: HTMLButtonElement | null = null
  private readonly follow = new FollowController({
    onStateChange: () => { this.dock() },
    onUserIntent: () => {
      this.pressedAnchor = undefined
      if (this.frame !== undefined) cancelAnimationFrame(this.frame)
      this.frame = undefined
      this.scheduleCapture()
    },
    onUserScroll: () => {
      if (this.element === null) return
      if (this.follow.attached) this.anchor = undefined
      else this.capture()
    },
    schedule: () => this.schedule(),
  })
  private get following() { return this.follow.attached }
  private announcement: HTMLSpanElement | null = null
  private anchor: { element: HTMLElement; offset: number } | undefined
  private frame: number | undefined
  private captureFrame: number | undefined
  /** Pointers down somewhere on the page; the dock keeps its layout until all are released. */
  private readonly pressed = new Set<number>()
  private pressedAnchor: { pointerId: number; element: HTMLElement; offset: number } | undefined
  readonly attachJump = (button: HTMLButtonElement | null) => {
    this.jumpButton = button
    this.dock()
  }

  /** One detached-state affordance; its dock never reflows the lane under an active press. */
  private dock() {
    if (this.jumpButton !== null && this.pressed.size === 0) this.jumpButton.hidden = this.following
    if (this.announcement !== null) this.announcement.textContent = this.following ? '' : 'Reading earlier messages. Jump to latest is available.'
    if (this.element !== null) this.element.dataset.followState = this.following ? 'attached' : 'detached'
  }

  readonly attachAnnouncement = (element: HTMLSpanElement | null) => {
    this.announcement = element
    this.dock()
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
      if (this.following) this.writeTop(element.scrollHeight)
      else if (this.captureFrame === undefined && this.anchor?.element.isConnected) {
        this.writeTop(element.scrollTop + this.anchor.element.getBoundingClientRect().top - element.getBoundingClientRect().top - this.anchor.offset)
      }
    })
  }

  readonly jump = () => {
    this.anchor = undefined
    this.pressedAnchor = undefined
    this.follow.jump()
    this.dock()
  }

  readonly attach = (element: HTMLDivElement | null) => {
    if (element === null) return
    this.element = element
    const detachFollow = this.follow.attach(element)
    this.dock()
    // Document-wide, so presses that start anywhere (a row action included) defer the reveal.
    const page = element.ownerDocument
    const view = page.defaultView
    const press = (event: PointerEvent) => {
      this.pressed.add(event.pointerId)
      if (this.pressedAnchor !== undefined || !(event.target instanceof Element)) return
      const row = event.target.closest<HTMLElement>(rowSelector)
      if (row !== null && element.contains(row)) this.pressedAnchor = { pointerId: event.pointerId, element: row, offset: row.getBoundingClientRect().top - element.getBoundingClientRect().top }
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
  /** Changing conversations always opens at the live edge. */
  readonly stateKey?: string
  /** Host command: a changed, defined key resumes following; an unchanged key never scrolls. */
  readonly scrollToBottomKey?: string
}

export const EmbraceScrollViewport = React.memo(function EmbraceScrollViewport({ items, children, contentProps, stateKey, scrollToBottomKey, ...props }: EmbraceScrollViewportProps) {
  const [controller] = React.useState(() => new ViewportController())
  const previousItems = React.useRef(items)
  const previousKey = React.useRef(stateKey)
  const previousCommand = React.useRef(scrollToBottomKey)
  React.useLayoutEffect(() => {
    if (stateKey !== previousKey.current) {
      previousKey.current = stateKey
      controller.jump()
    } else if (scrollToBottomKey !== undefined && scrollToBottomKey !== previousCommand.current) {
      // Following persists, so rows that commit after the command (the pending send) stay in view.
      controller.jump()
    } else if (previousItems.current !== items) controller.schedule()
    // A conversation switch starts attached and adopts its own send key.
    previousCommand.current = scrollToBottomKey
    previousItems.current = items
  }, [controller, items, stateKey, scrollToBottomKey])
  // Every layout commit can insert above a pressed row, including runtime adoption without new items.
  React.useLayoutEffect(() => { controller.preservePress() })
  return <div {...stylex.props(styles.frame)}>
    <div {...props} style={{ ...props.style, overflowAnchor: 'none' }} ref={controller.attach}><div {...contentProps}>{children}</div></div>
    <Button ref={controller.attachJump} onPress={controller.jump} hidden {...stylex.props(styles.jump)}>New messages ↓</Button>
    <span ref={controller.attachAnnouncement} role="status" aria-live="polite" {...stylex.props(styles.announcement)} />
  </div>
})

const styles = stylex.create({
  frame: { display: 'flex', flexDirection: 'column', flex: '1 1 0', minHeight: 0, minWidth: 0 },
  // A visible jump control gets its own dock, never covering a reader's current line.
  jump: { flexShrink: 0, marginInline: 'auto', marginBlock: spaceVars.md, paddingBlock: spaceVars.xs, paddingInline: spaceVars.md, borderRadius: radiusVars.full, borderWidth: spaceVars.hairline, borderStyle: 'solid', borderColor: borderVars.borderStrong, backgroundColor: surfaceVars.raised, color: textVars.fg, fontSize: typeVars.metaSize, cursor: 'pointer', ':focus-visible': { outlineWidth: spaceVars.xs2, outlineStyle: 'solid', outlineColor: borderVars.borderStrong, outlineOffset: spaceVars.xs2 } },
  announcement: { position: 'absolute', width: 1, height: 1, overflow: 'hidden', clipPath: 'inset(50%)', whiteSpace: 'nowrap' },
})
