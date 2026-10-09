import type { ListLayout } from 'react-aria-components'
import type { ViewportState, ViewportStore } from '../EmbraceScrollViewport'
import { FollowController } from './FollowController'
import { captureReadingAnchor, resolveReadingAnchor, type ReadingAnchor } from './ReadingAnchor'
import { ViewportPublisher } from './ViewportPublisher'

type Row = { readonly id: string }

/** RAC geometry adapter; follow intent, visibility and motion are shared with the measured lane. */
export class ScrollController {
  private element: HTMLDivElement | null = null
  private anchor: ReadingAnchor | undefined
  private pending: ViewportState | undefined
  private lastTop = 0
  private frame: number | undefined
  private rowKeys = new Set<string>()
  private readonly follow: FollowController
  private readonly publisher: ViewportPublisher

  constructor(private readonly options: {
    readonly layout: ListLayout<unknown>
    readonly saved?: ViewportState
    readonly onVisibilityChange: (visible: boolean) => void
  }) {
    this.publisher = new ViewportPublisher(() => this.released(true), options.saved)
    this.follow = new FollowController({
      onStateChange: () => {
        if (this.follow.attached) this.anchor = undefined
        if (this.element !== null) this.element.dataset.followState = this.follow.attached ? 'attached' : 'detached'
      },
      onVisibilityChange: () => this.options.onVisibilityChange(this.follow.showJump),
      onUserIntent: () => {
        this.publisher.readerIntent()
        if (this.frame !== undefined) cancelAnimationFrame(this.frame)
        this.frame = undefined
        this.pending = undefined
        if (this.element !== null) this.anchor = captureReadingAnchor(this.element)
      },
      onUserScroll: () => {
        if (this.element === null) return
        this.lastTop = this.element.scrollTop
        if (!this.follow.attached) this.anchor = captureReadingAnchor(this.element)
        this.publisher.readerScroll()
      },
      schedule: () => this.schedule(),
    })
    if (options.saved !== undefined && !options.saved.following) { this.pending = options.saved; this.follow.read() }
  }

  readonly bindStore = (store?: ViewportStore, key?: string) => { this.publisher.bind(store, key) }
  readonly setRunning = (running: boolean) => this.follow.setRunning(running)
  readonly released = (active = false): ViewportState => ({
    top: active ? this.element?.scrollTop ?? this.lastTop : this.lastTop, following: this.follow.attached, unread: this.follow.showJump, updatedAt: this.publisher.updatedAt,
    anchor: this.anchor === undefined ? undefined : { rowId: this.anchor.rowId, text: this.anchor.text, offset: this.anchor.offset },
  })
  readonly jump = () => {
    this.anchor = undefined
    this.pending = undefined
    this.follow.jump()
    this.publisher.reattach()
  }
  readonly activate = () => {
    this.anchor = undefined
    this.pending = undefined
    this.follow.jump(true)
    this.publisher.reattach()
  }

  /** React 19 cleans up the ref when the actual RAC scroll element unmounts. */
  readonly attach = (element: HTMLDivElement | null) => {
    if (element === null) return
    this.element = element
    element.dataset.followState = this.follow.attached ? 'attached' : 'detached'
    const detach = this.follow.attach(element)
    const observer = new ResizeObserver(() => this.schedule())
    observer.observe(element)
    if (element.firstElementChild !== null) observer.observe(element.firstElementChild)
    this.schedule()
    return () => {
      this.lastTop = element.scrollTop
      detach()
      observer.disconnect()
      this.publisher.detach()
      if (this.frame !== undefined) cancelAnimationFrame(this.frame)
      this.frame = undefined
      this.element = null
    }
  }

  private schedule() {
    if (this.frame !== undefined) return
    this.frame = requestAnimationFrame(() => {
      this.frame = undefined
      const element = this.element
      if (element === null) return
      if (!this.follow.attached) {
        const saved = this.pending?.anchor ?? this.anchor
        if (saved !== undefined && this.rowKeys.has(saved.rowId)) {
          const info = this.options.layout.getLayoutInfo(saved.rowId)
          if (info === null) return
          const connected = this.anchor?.element.isConnected === true ? this.anchor : undefined
          if (connected === undefined) element.scrollTop = info.rect.y - saved.offset
          const anchor = connected ?? resolveReadingAnchor(element, saved)
          if (anchor !== undefined) {
            element.scrollTop += anchor.element.getBoundingClientRect().top - element.getBoundingClientRect().top - anchor.offset
            this.anchor = anchor
            this.pending = undefined
          }
          this.follow.markWrite(element.scrollTop)
        } else if (this.pending !== undefined && this.pending.anchor === undefined) {
          element.scrollTop = this.pending.top
          this.follow.markWrite(element.scrollTop)
          this.anchor = captureReadingAnchor(element)
          this.pending = undefined
        }
      }
      this.follow.pin(element)
      this.lastTop = element.scrollTop
    })
  }

  /** Collection publication and ResizeObserver share one geometry reconciliation. */
  afterRowsChange(rows: readonly Row[]) {
    this.rowKeys.clear()
    for (const row of rows) this.rowKeys.add(row.id)
    this.schedule()
  }
}
