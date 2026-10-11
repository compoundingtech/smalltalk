import { motionNumbers } from '../composition-tokens.stylex'

/** Only an explicit return-to-end animates; ordinary following still settles before paint. */
export class FollowAnimation {
  private element: HTMLElement | null = null
  private frame: number | undefined
  private started = 0
  private from = 0
  private motion: MediaQueryList | undefined

  constructor(private readonly writeTop: (top: number) => void) {}

  get active(): boolean { return this.element !== null }

  start(element: HTMLElement) {
    this.cancel()
    const motion = window.matchMedia('(prefers-reduced-motion: reduce)')
    if (motion.matches) {
      this.writeTop(Math.max(0, element.scrollHeight - element.clientHeight))
      return
    }
    this.motion = motion
    motion.addEventListener('change', this.motionChanged)
    this.element = element
    this.from = element.scrollTop
    this.started = performance.now()
    this.frame = requestAnimationFrame(this.step)
  }

  cancel(): boolean {
    const active = this.active
    if (this.frame !== undefined) cancelAnimationFrame(this.frame)
    this.frame = undefined
    this.motion?.removeEventListener('change', this.motionChanged)
    this.motion = undefined
    this.element = null
    return active
  }

  private readonly motionChanged = (event: MediaQueryListEvent) => {
    const element = this.element
    if (!event.matches || element === null) return
    this.cancel()
    this.writeTop(Math.max(0, element.scrollHeight - element.clientHeight))
  }

  private readonly step = (time: number) => {
    const element = this.element
    if (element === null) return
    const progress = Math.min(1, Math.max(0, (time - this.started) / motionNumbers.standard))
    const eased = 1 - (1 - progress) ** 3
    // Re-read the live edge every frame: streaming and virtual measurements may move it.
    const end = Math.max(0, element.scrollHeight - element.clientHeight)
    this.writeTop(this.from + (end - this.from) * eased)
    if (progress < 1) this.frame = requestAnimationFrame(this.step)
    else this.cancel()
  }
}
