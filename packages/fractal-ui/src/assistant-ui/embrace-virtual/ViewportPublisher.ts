import type { ViewportState, ViewportStore } from '../EmbraceScrollViewport'
import { nextViewportStamp } from './ViewportStamp'
const READER_PUBLISH_MS = 250

/** Publishes live reader ownership without letting an unmount overwrite newer hydrated memory. */
export class ViewportPublisher {
  updatedAt: number
  private store: ViewportStore | undefined
  private key: string | undefined
  private untrack: (() => void) | undefined
  private intentFrame: number | undefined
  private timer: number | undefined
  private lastPublish = -Infinity

  constructor(private readonly read: () => ViewportState, saved?: ViewportState) {
    this.updatedAt = saved?.updatedAt ?? nextViewportStamp()
  }
  readonly bind = (store?: ViewportStore, key?: string) => {
    if (store === this.store && key === this.key) return
    this.detach()
    this.store = store
    this.key = key
    this.lastPublish = -Infinity
    if (store !== undefined && key !== undefined) this.untrack = store.trackViewport(key, this.read)
  }
  readonly restore = (saved?: ViewportState) => { this.updatedAt = saved?.updatedAt ?? nextViewportStamp() }
  readonly readerIntent = () => {
    this.updatedAt = nextViewportStamp()
    if (this.store === undefined || this.key === undefined || this.intentFrame !== undefined) return
    this.intentFrame = requestAnimationFrame(() => { this.intentFrame = undefined; this.notify() })
  }
  readonly readerScroll = () => { this.updatedAt = nextViewportStamp(); this.notify() }
  readonly reattach = () => {
    this.updatedAt = nextViewportStamp()
    clearTimeout(this.timer)
    this.timer = undefined
    if (this.intentFrame !== undefined) cancelAnimationFrame(this.intentFrame)
    this.intentFrame = undefined
    this.publish()
  }
  private notify() {
    if (this.store === undefined || this.key === undefined) return
    const remaining = READER_PUBLISH_MS - (performance.now() - this.lastPublish)
    if (remaining <= 0) { clearTimeout(this.timer); this.timer = undefined; this.publish() }
    else if (this.timer === undefined) this.timer = window.setTimeout(() => { this.timer = undefined; this.publish() }, remaining)
  }
  private publish() {
    if (this.store === undefined || this.key === undefined) return
    this.lastPublish = performance.now()
    this.store.save(this.key, this.read())
  }
  readonly detach = () => {
    clearTimeout(this.timer)
    this.timer = undefined
    if (this.intentFrame !== undefined) cancelAnimationFrame(this.intentFrame)
    this.intentFrame = undefined
    this.untrack?.()
    this.untrack = undefined
    this.store = undefined
    this.key = undefined
  }
}
