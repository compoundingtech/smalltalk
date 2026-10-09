import type { ListLayout } from 'react-aria-components'
import { FollowController, type FollowState } from './FollowController'

type Row = { readonly id: string }
type ReadingPosition = { readonly key: string; readonly offset: number }

/** RAC geometry adapter; follow transitions are owned by the same controller as the DOM lane. */
export class ScrollController {
  private element: HTMLDivElement | null = null
  private anchor: ReadingPosition | undefined
  private frame: number | undefined
  private rowKeys = new Set<string>()
  private readonly follow: FollowController

  constructor(private readonly options: {
    readonly layout: ListLayout<unknown>
    readonly onStateChange: (state: FollowState) => void
  }) {
    this.follow = new FollowController({
      onStateChange: state => {
        if (state._tag === 'Attached') this.anchor = undefined
        this.options.onStateChange(state)
      },
      onUserIntent: () => {
        if (this.frame !== undefined) cancelAnimationFrame(this.frame)
        this.frame = undefined
        this.captureAnchor()
      },
      onUserScroll: () => { if (!this.follow.attached) this.captureAnchor() },
      schedule: () => this.schedule(),
    })
  }

  readonly jump = () => {
    this.anchor = undefined
    this.follow.jump()
  }

  /** React 19 cleans up the ref when the actual RAC scroll element unmounts. */
  readonly attach = (element: HTMLDivElement | null) => {
    if (element === null) return
    this.element = element
    const detach = this.follow.attach(element)
    const observer = new ResizeObserver(() => this.schedule())
    observer.observe(element)
    if (element.firstElementChild !== null) observer.observe(element.firstElementChild)
    this.schedule()
    return () => {
      detach()
      observer.disconnect()
      if (this.frame !== undefined) cancelAnimationFrame(this.frame)
      this.frame = undefined
      this.element = null
    }
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

  private schedule() {
    if (this.frame !== undefined) return
    this.frame = requestAnimationFrame(() => {
      this.frame = undefined
      const element = this.element
      if (element === null) return
      if (this.follow.attached) element.scrollTop = element.scrollHeight
      else if (this.anchor !== undefined && this.rowKeys.has(this.anchor.key)) {
        const info = this.options.layout.getLayoutInfo(this.anchor.key)
        if (info === null) return
        element.scrollTop = info.rect.y - this.anchor.offset
        const row = element.querySelector<HTMLElement>(`[data-embrace-entry-id="${CSS.escape(this.anchor.key)}"]`)
        if (row !== null) element.scrollTop += row.getBoundingClientRect().top - element.getBoundingClientRect().top - this.anchor.offset
      }
      this.follow.markWrite(element.scrollTop)
    })
  }

  /** Collection publication and ResizeObserver share one geometry reconciliation. */
  afterRowsChange(rows: readonly Row[]) {
    this.rowKeys.clear()
    for (const row of rows) this.rowKeys.add(row.id)
    this.schedule()
  }
}
