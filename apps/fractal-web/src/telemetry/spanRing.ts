/**
 * In-memory tail of finished spans plus browser vitals, read by the dev perf panel. Notifications
 * are throttled to 4 Hz so the panel never competes with the interactions it measures.
 */

/** A completed span projected for panel display, with times relative to the page timeline. */
export interface RingSpan {
  readonly name: string
  readonly label: string
  readonly startMs: number
  readonly durationMs: number
  readonly attributes: Readonly<Record<string, unknown>>
}

/** Page-level responsiveness and layout-stability measurements accumulated during the session. */
export interface Vitals {
  /** Worst Event Timing interaction so far (INP for < 50 interactions), ms. */
  readonly inpMs: number | undefined
  readonly lcpMs: number | undefined
  readonly cls: number
  readonly longFrames: number
  readonly longFrameMaxMs: number
}

/** Immutable read model consumed by external-store subscribers. */
export interface RingSnapshot {
  readonly spans: ReadonlyArray<RingSpan>
  readonly vitals: Vitals
}

/** Bounded live telemetry store shared by the tracer tee and observers. */
export interface SpanRing {
  readonly push: (span: RingSpan) => void
  readonly updateVitals: (patch: Partial<Vitals>) => void
  readonly getSnapshot: () => RingSnapshot
  readonly subscribe: (listener: () => void) => () => void
}

/** Creates an empty bounded ring whose subscribers are notified at most four times per second. */
export const makeSpanRing = (): SpanRing => {
  let snapshot: RingSnapshot = {
    spans: [],
    vitals: { inpMs: undefined, lcpMs: undefined, cls: 0, longFrames: 0, longFrameMaxMs: 0 },
  }
  let buffer: RingSpan[] = []
  const listeners = new Set<() => void>()
  let notifyScheduled = false
  const schedule = () => {
    if (notifyScheduled) return
    notifyScheduled = true
    setTimeout(() => {
      notifyScheduled = false
      snapshot = { ...snapshot, spans: buffer.slice() }
      for (const listener of listeners) listener()
    }, notifyEveryMs)
  }
  return {
    push: (span) => {
      buffer.push(span)
      if (buffer.length > capacity) buffer = buffer.slice(-capacity)
      schedule()
    },
    updateVitals: (patch) => {
      snapshot = { ...snapshot, vitals: { ...snapshot.vitals, ...patch } }
      schedule()
    },
    getSnapshot: () => snapshot,
    subscribe: (listener) => {
      listeners.add(listener)
      return () => {
        listeners.delete(listener)
      }
    },
  }
}

const capacity = 200
const notifyEveryMs = 250
