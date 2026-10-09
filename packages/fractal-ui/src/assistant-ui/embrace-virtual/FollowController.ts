/** The live edge band is distinct from the subpixel tolerance used for geometry writes. */
export const FOLLOW_BOTTOM_THRESHOLD = 40
const USER_SCROLL_WINDOW_MS = 250
const AFFORDANCE_DELAY_MS = 150
const AFFORDANCE_MOTION_MS = 470
const STREAM_MOTION_MS = 250
const scrollKeys: Readonly<Record<string, true>> = { PageUp: true, PageDown: true, Home: true, End: true, ArrowUp: true, ArrowDown: true, ' ': true }
export type FollowState = { readonly _tag: 'Attached' } | { readonly _tag: 'Detached' }

/** Shared intent, visibility and motion controller; adapters own reading-anchor geometry only. */
export class FollowController {
  private state: FollowState = { _tag: 'Attached' }
  private element: HTMLDivElement | null = null
  private inputUntil = 0
  private direction = 0
  private dragging = false
  private lastTop = 0
  private touchY: number | undefined
  private writtenTop: number | undefined
  private revealTimer: number | undefined
  private visible = false
  private running = false
  private instant = true
  private requestedMotion: number | undefined
  private motionFrame: number | undefined
  private motionKind: 'stream' | 'jump' | undefined
  private reducedMotion: MediaQueryList | undefined

  constructor(private readonly options: {
    readonly onStateChange: (state: FollowState) => void
    readonly onVisibilityChange: () => void
    readonly onUserIntent: () => void
    readonly onUserScroll: () => void
    readonly schedule: () => void
  }) {}

  get attached() { return this.state._tag === 'Attached' }
  get showJump() { return this.visible }
  private setAttached(attached: boolean) {
    if (attached !== this.attached) {
      this.state = { _tag: attached ? 'Attached' : 'Detached' }
      this.options.onStateChange(this.state)
    }
    this.refreshVisibility()
  }
  private refreshVisibility() {
    const element = this.element
    const shouldReveal = !this.attached && element !== null && element.scrollHeight - element.clientHeight - element.scrollTop > FOLLOW_BOTTOM_THRESHOLD
    if (!shouldReveal) {
      clearTimeout(this.revealTimer)
      this.revealTimer = undefined
      if (this.visible) { this.visible = false; this.options.onVisibilityChange() }
    } else if (!this.visible && this.revealTimer === undefined) {
      this.revealTimer = window.setTimeout(() => {
        this.revealTimer = undefined
        if (!this.attached && this.element !== null && this.element.scrollHeight - this.element.clientHeight - this.element.scrollTop > FOLLOW_BOTTOM_THRESHOLD) {
          this.visible = true
          this.options.onVisibilityChange()
        }
      }, AFFORDANCE_DELAY_MS)
    }
  }
  private cancelMotion() {
    if (this.motionFrame !== undefined) cancelAnimationFrame(this.motionFrame)
    this.motionFrame = undefined
    this.motionKind = undefined
    this.requestedMotion = undefined
  }
  /** Holding a row action pauses catch-up without changing the follow state. */
  readonly pauseMotion = () => { this.cancelMotion() }
  readonly setRunning = (running: boolean) => {
    if (this.running === running) return
    this.running = running
    if (!running && this.motionKind === 'stream') this.cancelMotion()
    this.options.schedule()
  }
  readonly read = () => {
    this.cancelMotion()
    this.setAttached(false)
  }
  readonly jump = (animate = false) => {
    this.inputUntil = 0
    this.dragging = false
    this.cancelMotion()
    this.instant = !animate
    this.requestedMotion = animate ? AFFORDANCE_MOTION_MS : undefined
    this.setAttached(true)
    this.options.schedule()
  }
  readonly markWrite = (top: number) => { this.writtenTop = top; this.lastTop = top; this.refreshVisibility() }

  /** Called after anchor/press compensation. Growth changes the target, never the attachment state. */
  readonly pin = (element: HTMLDivElement) => {
    this.refreshVisibility()
    if (!this.attached || this.motionFrame !== undefined) return
    const end = Math.max(0, element.scrollHeight - element.clientHeight)
    const reduced = this.reducedMotion?.matches === true
    const duration = this.instant || reduced ? 0 : this.requestedMotion ?? (this.running ? STREAM_MOTION_MS : 0)
    if (element.scrollHeight > element.clientHeight) this.instant = false
    this.requestedMotion = undefined
    if (duration === 0 || Math.abs(end - element.scrollTop) < 1) {
      element.scrollTop = end
      this.markWrite(element.scrollTop)
      return
    }
    const from = element.scrollTop
    const start = performance.now()
    this.motionKind = duration === AFFORDANCE_MOTION_MS ? 'jump' : 'stream'
    const step = (now: number) => {
      if (!this.attached || this.element !== element) { this.cancelMotion(); return }
      const progress = Math.min(1, (now - start) / duration)
      const target = Math.max(0, element.scrollHeight - element.clientHeight)
      element.scrollTop = from + (target - from) * (1 - (1 - progress) ** 3)
      this.markWrite(element.scrollTop)
      if (progress < 1) this.motionFrame = requestAnimationFrame(step)
      else { this.motionFrame = undefined; this.motionKind = undefined }
    }
    this.motionFrame = requestAnimationFrame(step)
  }

  readonly attach = (element: HTMLDivElement) => {
    this.element = element
    this.lastTop = element.scrollTop
    this.reducedMotion = element.ownerDocument.defaultView?.matchMedia('(prefers-reduced-motion: reduce)')
    const intent = (event: Event) => {
      let direction = 0
      if (event instanceof KeyboardEvent) {
        if (!scrollKeys[event.key] || (event.target instanceof Element && event.target.closest('input,textarea,button,a,[contenteditable="true"],[role="button"]'))) return
        direction = ['PageUp', 'Home', 'ArrowUp'].includes(event.key) || event.key === ' ' && event.shiftKey ? -1 : 1
      } else if (event instanceof WheelEvent) {
        if (event.deltaY === 0) return
        direction = Math.sign(event.deltaY)
      } else if (event instanceof TouchEvent) {
        const y = event.touches[0]?.clientY
        direction = y === undefined || this.touchY === undefined ? 0 : Math.sign(this.touchY - y)
        this.touchY = y
      } else if (event instanceof PointerEvent) {
        const rect = element.getBoundingClientRect()
        if (event.target !== element || event.clientX < rect.left + element.clientLeft + element.clientWidth) return
        this.dragging = true
      }
      this.cancelMotion()
      this.direction = direction
      this.inputUntil = performance.now() + USER_SCROLL_WINDOW_MS
      this.writtenTop = undefined
      this.options.onUserIntent()
      // Upward intent owns the line even before it leaves the live-edge band.
      if (direction < 0) this.setAttached(false)
    }
    const scroll = () => {
      const top = element.scrollTop
      if (this.writtenTop !== undefined && Math.abs(top - this.writtenTop) < 1) {
        this.writtenTop = undefined
        this.lastTop = top
        this.refreshVisibility()
        return
      }
      const user = this.dragging || performance.now() < this.inputUntil
      if (user) {
        const direction = this.dragging || this.direction === 0 ? Math.sign(top - this.lastTop) : this.direction
        this.inputUntil = performance.now() + USER_SCROLL_WINDOW_MS
        if (direction < 0) this.setAttached(false)
        else if (direction > 0 && element.scrollHeight - element.clientHeight - top <= FOLLOW_BOTTOM_THRESHOLD) {
          this.setAttached(true)
          this.instant = true
          this.options.schedule()
        }
        this.options.onUserScroll()
      } else if (this.attached) this.options.schedule()
      this.lastTop = top
      this.refreshVisibility()
    }
    const touchStart = (event: TouchEvent) => { this.touchY = event.touches[0]?.clientY }
    const end = () => { this.dragging = false; this.inputUntil = 0; this.direction = 0; this.touchY = undefined }
    const inputs = ['wheel', 'touchmove', 'keydown', 'pointerdown'] as const
    for (const type of inputs) element.addEventListener(type, intent, { passive: true })
    element.addEventListener('touchstart', touchStart, { passive: true })
    element.addEventListener('scroll', scroll, { passive: true })
    element.addEventListener('scrollend', end, { passive: true })
    element.ownerDocument.addEventListener('pointerup', end, true)
    element.ownerDocument.addEventListener('pointercancel', end, true)
    element.ownerDocument.defaultView?.addEventListener('blur', end)
    this.refreshVisibility()
    return () => {
      for (const type of inputs) element.removeEventListener(type, intent)
      element.removeEventListener('touchstart', touchStart)
      element.removeEventListener('scroll', scroll)
      element.removeEventListener('scrollend', end)
      element.ownerDocument.removeEventListener('pointerup', end, true)
      element.ownerDocument.removeEventListener('pointercancel', end, true)
      element.ownerDocument.defaultView?.removeEventListener('blur', end)
      clearTimeout(this.revealTimer)
      this.revealTimer = undefined
      this.cancelMotion()
      this.element = null
      this.reducedMotion = undefined
      end()
    }
  }
}
