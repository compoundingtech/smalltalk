/** Distance from the latest edge at which reader scrolling resumes live following. */
export const FOLLOW_BOTTOM_THRESHOLD = 48
const USER_SCROLL_WINDOW_MS = 250
const scrollKeys: Readonly<Record<string, true>> = { PageUp: true, PageDown: true, Home: true, End: true, ArrowUp: true, ArrowDown: true, ' ': true }

export type FollowState = { readonly _tag: 'Attached' } | { readonly _tag: 'Detached' }

/** One user-intent state machine shared by the measured and virtual transcript adapters. */
export class FollowController {
  private state: FollowState = { _tag: 'Attached' }
  private inputUntil = 0
  private dragging = false
  private writtenTop: number | undefined

  constructor(private readonly options: {
    readonly onStateChange: (state: FollowState) => void
    readonly onUserIntent: () => void
    readonly onUserScroll: () => void
    readonly schedule: () => void
  }) {}

  get attached() { return this.state._tag === 'Attached' }
  private setAttached(attached: boolean) {
    if (attached === this.attached) return
    this.state = { _tag: attached ? 'Attached' : 'Detached' }
    this.options.onStateChange(this.state)
  }
  readonly jump = () => {
    this.inputUntil = 0
    this.dragging = false
    this.setAttached(true)
    this.options.schedule()
  }
  readonly markWrite = (top: number) => { this.writtenTop = top }
  readonly attach = (element: HTMLDivElement) => {
    const intent = (event: Event) => {
      let away = false
      if (event instanceof KeyboardEvent) {
        if (scrollKeys[event.key] !== true || (event.target instanceof Element && event.target.closest('input,textarea,button,a,[contenteditable="true"],[role="button"]'))) return
        away = ['PageUp', 'Home', 'ArrowUp'].includes(event.key) || event.key === ' ' && event.shiftKey
      } else if (event instanceof WheelEvent) {
        if (event.deltaY === 0) return
        away = event.deltaY < 0
      } else if (event instanceof PointerEvent) {
        // A row press is not scrolling. Only a press on the native scrollbar owns subsequent scrolls.
        const rect = element.getBoundingClientRect()
        if (event.target !== element || event.clientX < rect.left + element.clientLeft + element.clientWidth) return
        this.dragging = true
      }
      this.inputUntil = performance.now() + USER_SCROLL_WINDOW_MS
      this.writtenTop = undefined
      this.options.onUserIntent()
      if (away && element.scrollTop > 0) this.setAttached(false)
    }
    const scroll = () => {
      if (this.writtenTop !== undefined && Math.abs(element.scrollTop - this.writtenTop) < 1) {
        this.writtenTop = undefined
        return
      }
      const user = this.dragging || performance.now() < this.inputUntil
      if (user) {
        this.inputUntil = performance.now() + USER_SCROLL_WINDOW_MS
        this.setAttached(element.scrollHeight - element.clientHeight - element.scrollTop <= FOLLOW_BOTTOM_THRESHOLD)
        this.options.onUserScroll()
      } else if (this.attached) this.options.schedule()
    }
    const end = () => { this.dragging = false; this.inputUntil = 0 }
    const inputs = ['wheel', 'touchmove', 'keydown', 'pointerdown'] as const
    for (const type of inputs) element.addEventListener(type, intent, { passive: true })
    element.addEventListener('scroll', scroll, { passive: true })
    element.addEventListener('scrollend', end, { passive: true })
    element.ownerDocument.addEventListener('pointerup', end, true)
    element.ownerDocument.addEventListener('pointercancel', end, true)
    element.ownerDocument.defaultView?.addEventListener('blur', end)
    return () => {
      for (const type of inputs) element.removeEventListener(type, intent)
      element.removeEventListener('scroll', scroll)
      element.removeEventListener('scrollend', end)
      element.ownerDocument.removeEventListener('pointerup', end, true)
      element.ownerDocument.removeEventListener('pointercancel', end, true)
      element.ownerDocument.defaultView?.removeEventListener('blur', end)
      end()
    }
  }
}
