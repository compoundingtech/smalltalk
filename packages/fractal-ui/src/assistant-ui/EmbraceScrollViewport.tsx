import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button } from 'react-aria-components'
import { surfaceVars, textVars, borderVars, radiusVars, spaceVars, typeVars, geometryNumbers } from './composition-tokens.stylex'

const rowSelector = '[data-item-id], [data-embrace-entry-id]'
const navigationKeys: Readonly<Record<string, true>> = { PageUp: true, PageDown: true, Home: true, End: true, ArrowUp: true, ArrowDown: true, ' ': true }

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

  readonly attachJump = (button: HTMLButtonElement | null) => {
    this.jumpButton = button
    if (button !== null) button.hidden = !this.unread
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
      else if (this.captureFrame === undefined && this.anchor?.element.isConnected) {
        this.writeTop(element.scrollTop + this.anchor.element.getBoundingClientRect().top - element.getBoundingClientRect().top - this.anchor.offset)
      }
      this.lastWidth = element.clientWidth
    })
  }

  readonly jump = () => {
    this.following = true
    this.unread = false
    this.anchor = undefined
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
}

export const EmbraceScrollViewport = React.memo(function EmbraceScrollViewport({ items, children, contentProps, ...props }: EmbraceScrollViewportProps) {
  const [controller] = React.useState(() => new ViewportController())
  const previousItems = React.useRef(items)
  React.useLayoutEffect(() => {
    if (previousItems.current !== items) controller.changed()
    previousItems.current = items
  }, [controller, items])
  return <div {...stylex.props(styles.frame)}>
    <div {...props} ref={controller.attach}><div {...contentProps}>{children}</div></div>
    <Button ref={controller.attachJump} onPress={controller.jump} hidden {...stylex.props(styles.jump)}>New messages ↓</Button>
  </div>
})

const styles = stylex.create({
  frame: { display: 'flex', flexDirection: 'column', flex: '1 1 0', minHeight: 0, minWidth: 0 },
  // A visible jump control gets its own dock, never covering a reader's current line.
  jump: { flexShrink: 0, marginInline: 'auto', marginBlock: spaceVars.md, paddingBlock: spaceVars.xs, paddingInline: spaceVars.md, borderRadius: radiusVars.full, borderWidth: spaceVars.hairline, borderStyle: 'solid', borderColor: borderVars.borderStrong, backgroundColor: surfaceVars.raised, color: textVars.fg, fontSize: typeVars.metaSize, cursor: 'pointer' },
})
