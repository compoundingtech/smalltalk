// @vitest-environment jsdom
import { performance as timing } from 'node:perf_hooks'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cancelMeasure, markStart, measureEnd, workflows } from './perfMarks.ts'

let frames: Map<number, FrameRequestCallback>
let nextFrame: number
const paint = () => {
  const pending = [...frames.values()]
  frames.clear()
  pending.forEach(callback => callback(0))
}
const samples = (workflow: string) => timing.getEntriesByName(`fractal:${workflow}`, 'measure')
beforeEach(() => {
  vi.useFakeTimers()
  vi.stubGlobal('performance', timing)
  frames = new Map()
  nextFrame = 0
  vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => { frames.set(++nextFrame, callback); return nextFrame })
  vi.stubGlobal('cancelAnimationFrame', (id: number) => frames.delete(id))
})
afterEach(() => {
  workflows.forEach(workflow => { cancelMeasure(workflow); timing.clearMeasures(`fractal:${workflow}`) })
  vi.useRealTimers()
  vi.unstubAllGlobals()
})

describe('workflow User Timing', () => {
  it('publishes the five exact names, only after a frame and following task', () => {
    expect(workflows).toEqual(['sidebar_interactive', 'switch_to_tail', 'open_transcript', 'keystroke_paint', 'resources_panel'])
    for (const workflow of workflows) {
      markStart(workflow, { startTime: 0 })
      measureEnd(workflow)
      measureEnd(workflow)
      expect(samples(workflow)).toHaveLength(0)
      paint()
      expect(samples(workflow)).toHaveLength(0)
      vi.runOnlyPendingTimers()
      expect(samples(workflow)).toHaveLength(1)
      expect(samples(workflow)[0]!.startTime).toBe(0)
      expect(timing.getEntriesByName(`fractal:${workflow}:start`)).toHaveLength(0)
    }
  })
  it('cancels stale starts and queued ends, and matches the selected agent', () => {
    const cancelOld = markStart('switch_to_tail', { key: 'agent/old' })
    measureEnd('switch_to_tail', 'agent/old')
    paint()
    markStart('switch_to_tail', { key: 'agent/new' })
    cancelOld()
    cancelMeasure('switch_to_tail', 'agent/old')
    measureEnd('switch_to_tail', 'agent/old')
    vi.runOnlyPendingTimers()
    expect(samples('switch_to_tail')).toHaveLength(0)
    measureEnd('switch_to_tail', 'agent/new')
    paint()
    vi.runOnlyPendingTimers()
    expect(samples('switch_to_tail')).toHaveLength(1)
  })
  it('never completes an abandoned start or an end without a start', () => {
    measureEnd('resources_panel')
    const cancel = markStart('resources_panel')
    cancel()
    measureEnd('resources_panel')
    paint()
    vi.runOnlyPendingTimers()
    expect(samples('resources_panel')).toHaveLength(0)
  })
  it('cancels only the matching abandoned agent start', () => {
    markStart('open_transcript', { key: 'agent/selected' })
    measureEnd('open_transcript', 'agent/selected')
    cancelMeasure('open_transcript', 'agent/selected')
    measureEnd('open_transcript', 'agent/selected')
    paint()
    vi.runOnlyPendingTimers()
    expect(samples('open_transcript')).toHaveLength(0)
    expect(timing.getEntriesByName('fractal:open_transcript:start')).toHaveLength(0)
  })
  it('allows a cleaned-up commit effect to retry its pending paint', () => {
    markStart('open_transcript')
    measureEnd('open_transcript')()
    measureEnd('open_transcript')
    paint()
    vi.runOnlyPendingTimers()
    expect(samples('open_transcript')).toHaveLength(1)
  })
  it('bounds retained keystroke samples', () => {
    for (let key = 0; key < 205; key++) {
      markStart('keystroke_paint')
      measureEnd('keystroke_paint')
      paint()
      vi.runOnlyPendingTimers()
      expect(samples('keystroke_paint').length).toBeLessThanOrEqual(100)
    }
  })
  it('does nothing when performance is absent', () => {
    vi.stubGlobal('performance', undefined)
    expect(() => { markStart('sidebar_interactive')(); measureEnd('sidebar_interactive')(); cancelMeasure('sidebar_interactive') }).not.toThrow()
    expect(frames.size).toBe(0)
  })
})
