// From compoundingtech/smalltalk#1641, commit 4f74464e0b22ea889a09d9271d076c3c6fd85df8.
import { counters, type Counters } from './counters.ts'

export interface MeasurementResult {
  readonly frameDrops: number
  readonly framesCaptured: number
  readonly durationMs: number
  readonly averageFps: number
  readonly debugDelta: Readonly<Record<string, number>>
}
export interface FrameClock {
  readonly now: () => number
  readonly request: (callback: (time: number) => void) => number
  readonly cancel: (id: number) => void
}
export interface MeasurementEngine {
  readonly beginMeasure: () => number
  readonly endMeasure: (handle: number, options?: { readonly settleFrames?: number }) => Promise<MeasurementResult>
  readonly dispose: () => void
}
const browserClock: FrameClock = {
  now: () => performance.now(),
  request: (callback) => requestAnimationFrame(callback),
  cancel: (id) => cancelAnimationFrame(id),
}

/** A refresh period is explicit: do not confuse a slow host with a slow display. */
export const createMeasurementEngine = ({
  clock = browserClock,
  bag = counters,
  framePeriodMs = 1000 / 60,
}: {
  readonly clock?: FrameClock
  readonly bag?: Counters
  readonly framePeriodMs?: number
} = {}): MeasurementEngine => {
  if (!(framePeriodMs > 0) || !Number.isFinite(framePeriodMs)) throw new RangeError('Invalid refresh period')
  let nextHandle = 0
  const brackets = new Map<number, {
    started: number
    before: Readonly<Record<string, number>>
    previousFrame: number
    frames: number
    drops: number
    ending: boolean
    settle?: { remaining: number; resolve: () => void; reject: (error: Error) => void }
  }>()
  let request: number | undefined
  const sample = (time: number) => {
    request = undefined
    for (const bracket of brackets.values()) {
      bracket.drops += Math.max(0, Math.round((time - bracket.previousFrame) / framePeriodMs) - 1)
      bracket.previousFrame = time
      bracket.frames += 1
      if (bracket.settle !== undefined && --bracket.settle.remaining === 0) {
        bracket.settle.resolve()
        bracket.settle = undefined
      }
    }
    if (brackets.size > 0) request = clock.request(sample)
  }
  const beginMeasure = (): number => {
    const handle = ++nextHandle
    const started = clock.now()
    brackets.set(handle, { started, before: bag.snapshot(), previousFrame: started, frames: 0, drops: 0, ending: false })
    if (request === undefined) request = clock.request(sample)
    return handle
  }
  const endMeasure = async (handle: number, { settleFrames = 0 } = {}): Promise<MeasurementResult> => {
    const bracket = brackets.get(handle)
    if (bracket === undefined) throw new RangeError('Unknown measurement handle')
    if (!Number.isInteger(settleFrames) || settleFrames < 0) throw new RangeError('Invalid settle frame count')
    if (bracket.ending) throw new RangeError('Measurement already ending')
    bracket.ending = true
    if (settleFrames > 0) {
      await new Promise<void>((resolve, reject) => {
        bracket.settle = { remaining: settleFrames, resolve, reject }
      })
    }
    brackets.delete(handle)
    if (brackets.size === 0 && request !== undefined) {
      clock.cancel(request)
      request = undefined
    }
    const after = bag.snapshot()
    const debugDelta: Record<string, number> = {}
    for (const key of new Set([...Object.keys(bracket.before), ...Object.keys(after)])) {
      const delta = (after[key] ?? 0) - (bracket.before[key] ?? 0)
      if (delta !== 0) debugDelta[key] = delta
    }
    const durationMs = clock.now() - bracket.started
    return {
      frameDrops: bracket.drops,
      framesCaptured: bracket.frames,
      durationMs,
      averageFps: durationMs > 0 ? bracket.frames * 1000 / durationMs : 0,
      debugDelta,
    }
  }
  const dispose = (): void => {
    if (request !== undefined) clock.cancel(request)
    request = undefined
    for (const bracket of brackets.values()) bracket.settle?.reject(new RangeError('Measurement disposed'))
    brackets.clear()
  }
  return { beginMeasure, endMeasure, dispose }
}
