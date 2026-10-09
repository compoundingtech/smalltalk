import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button } from 'react-aria-components'
import { surfaceVars, textVars, borderVars, radiusVars, spaceVars, typeVars, geometryNumbers } from './composition-tokens.stylex'

const rowSelector = '[data-item-id], [data-embrace-entry-id]'
const navigationKeys: Readonly<Record<string, true>> = { PageUp: true, PageDown: true, Home: true, End: true, ArrowUp: true, ArrowDown: true, ' ': true }

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
  private lastWidth = 0
  private programmaticTop: number | undefined
  private restored: ViewportState | undefined
  /** A restored line whose content is settling; the first reader scroll clears it. */
  private pendingTop: number | undefined

  constructor(saved?: ViewportState) {
    if (saved !== undefined && !saved.following) {
      this.restored = saved
      this.unread = saved.unread
    }
  }

  readonly attachJump = (button: HTMLButtonElement | null) => {
    this.jumpButton = button
    if (button !== null) button.hidden = !this.unread
  }

  readonly released = (): ViewportState => ({ top: this.lastTop, following: this.following, unread: this.unread })

  /** Swaps a reused viewport to another conversation without carrying its unread mark across. */
  readonly resume = (saved?: ViewportState) => {
    this.unread = saved !== undefined && !saved.following && saved.unread
    if (this.jumpButton !== null) this.jumpButton.hidden = !this.unread
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

  readonly schedule = () => {
    if (this.frame !== undefined) return
    this.frame = requestAnimationFrame(() => {
      this.frame = undefined
      const element = this.element
      if (element === null) return
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
      this.lastWidth = element.clientWidth
    })
  }

  readonly jump = () => {
    this.following = true
    this.unread = false
    this.anchor = undefined
    this.pendingTop = undefined
    if (this.jumpButton !== null) this.jumpButton.hidden = true
    this.schedule()
  }

  readonly changed = () => {
    if (!this.following) {
      this.unread = true
      if (this.jumpButton !== null) this.jumpButton.hidden = false
    }
    this.schedule()
  }

  readonly attach = (element: HTMLDivElement | null) => {
    if (element === null) return
    this.element = element
    this.lastWidth = element.clientWidth
    const manual = (event: Event) => {
      if (event instanceof KeyboardEvent && (navigationKeys[event.key] !== true || (event.target instanceof HTMLElement && event.target.closest('input,textarea,[contenteditable="true"]')))) return
      this.following = false
      this.programmaticTop = undefined
      this.pendingTop = undefined
      this.scheduleCapture()
      if (this.frame !== undefined) cancelAnimationFrame(this.frame)
      this.frame = undefined
    }
    const scroll = () => {
      if (this.programmaticTop !== undefined && Math.abs(element.scrollTop - this.programmaticTop) < geometryNumbers.scrollEndTolerance) {
        this.programmaticTop = undefined
        return
      }
      // Reflow is not a request to follow. Width changes preserve the reader's anchor.
      if (element.clientWidth !== this.lastWidth) { this.schedule(); return }
      if (Math.abs(element.scrollTop - this.lastTop) < geometryNumbers.scrollEndTolerance) return
      if (this.pendingTop !== undefined) {
        this.writeTop(this.pendingTop)
        return
      }
      this.lastTop = element.scrollTop
      this.following = element.scrollHeight - element.clientHeight - element.scrollTop <= geometryNumbers.scrollEndTolerance
      if (this.following) {
        this.unread = false
        if (this.jumpButton !== null) this.jumpButton.hidden = true
        this.anchor = undefined
      } else this.scheduleCapture()
    }
    const observer = new ResizeObserver(this.schedule)
    observer.observe(element)
    if (element.firstElementChild !== null) observer.observe(element.firstElementChild)
    element.addEventListener('wheel', manual, { passive: true })
    element.addEventListener('touchmove', manual, { passive: true })
    element.addEventListener('keydown', manual)
    element.addEventListener('pointerdown', manual, { passive: true })
    element.addEventListener('focusin', manual)
    element.addEventListener('scroll', scroll, { passive: true })
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
      this.element = null
    }
  }
}

export interface EmbraceScrollViewportProps extends React.HTMLAttributes<HTMLDivElement> {
  readonly items: readonly { readonly id: string }[]
  readonly contentProps?: React.HTMLAttributes<HTMLDivElement>
  /** Each key keeps its own scroll state across viewport mounts. */
  readonly stateKey?: string
}

export const EmbraceScrollViewport = React.memo(function EmbraceScrollViewport({ items, children, contentProps, stateKey, ...props }: EmbraceScrollViewportProps) {
  const store = React.useContext(ViewportStoreContext)
  const [controller] = React.useState(() => new ViewportController(stateKey === undefined ? undefined : store?.get(stateKey)))
  const previousItems = React.useRef(items)
  const previousKey = React.useRef(stateKey)
  React.useLayoutEffect(() => {
    if (stateKey !== previousKey.current) {
      if (previousKey.current !== undefined) store?.save(previousKey.current, controller.released())
      previousKey.current = stateKey
      controller.resume(stateKey === undefined ? undefined : store?.get(stateKey))
    } else if (previousItems.current !== items) controller.changed()
    previousItems.current = items
  }, [controller, items, store, stateKey])
  // Mutation-phase saves precede the owning surface's layout effect that removes closed keys.
  React.useLayoutEffect(() => () => {
    if (previousKey.current !== undefined) store?.save(previousKey.current, controller.released())
  }, [controller, store])
  return <div {...stylex.props(styles.frame)}>
    <div {...props} style={{ ...props.style, overflowAnchor: 'none' }} ref={controller.attach}><div {...contentProps}>{children}</div></div>
    <Button ref={controller.attachJump} onPress={controller.jump} hidden {...stylex.props(styles.jump)}>New messages ↓</Button>
  </div>
})

const styles = stylex.create({
  frame: { display: 'flex', flexDirection: 'column', flex: '1 1 0', minHeight: 0, minWidth: 0 },
  // A visible jump control gets its own dock, never covering a reader's current line.
  jump: { flexShrink: 0, marginInline: 'auto', marginBlock: spaceVars.md, paddingBlock: spaceVars.xs, paddingInline: spaceVars.md, borderRadius: radiusVars.full, borderWidth: spaceVars.hairline, borderStyle: 'solid', borderColor: borderVars.borderStrong, backgroundColor: surfaceVars.raised, color: textVars.fg, fontSize: typeVars.metaSize, cursor: 'pointer' },
})
