export const workflows = ['sidebar_interactive', 'switch_to_tail', 'open_transcript', 'keystroke_paint', 'resources_panel'] as const
export type Workflow = typeof workflows[number]

const active = new Map<Workflow, { readonly performance: Performance; readonly key?: string; cancelPaint?: () => void }>()
const counts = new Map<Workflow, number>()
const noop = () => {}
const name = (workflow: Workflow) => `fractal:${workflow}`

/** Replacing a start cancels the unfinished sample, including an already queued paint. */
export const cancelMeasure = (workflow: Workflow, key?: string): void => {
  const operation = active.get(workflow)
  if (operation === undefined || (key !== undefined && operation.key !== key)) return
  operation.cancelPaint?.()
  operation.performance.clearMarks(`${name(workflow)}:start`)
  active.delete(workflow)
}

/** startTime=0 brackets document navigation, rather than module evaluation. Keys stay local. */
export const markStart = (workflow: Workflow, options: { readonly startTime?: number; readonly key?: string } = {}): (() => void) => {
  cancelMeasure(workflow)
  if (typeof performance === 'undefined' || typeof performance.mark !== 'function') return noop
  const operation = { performance, key: options.key }
  performance.mark(`${name(workflow)}:start`, options.startTime === undefined ? undefined : { startTime: options.startTime })
  active.set(workflow, operation)
  return () => { if (active.get(workflow) === operation) cancelMeasure(workflow) }
}

/** Call only after real content commits. rAF + task includes the next paint opportunity.
 * Cleanup cancels this paint, not the start, so StrictMode's repeated commit can retry it.
 * Retain at most 100 samples per workflow; observers still receive every completed sample.
 */
export const measureEnd = (workflow: Workflow, key?: string): (() => void) => {
  const operation = active.get(workflow)
  if (operation === undefined || operation.key !== key || operation.cancelPaint !== undefined || typeof requestAnimationFrame !== 'function') return noop
  let task: number | undefined
  const frame = requestAnimationFrame(() => {
    task = window.setTimeout(() => {
      if (active.get(workflow) !== operation) return
      const measureName = name(workflow)
      if ((counts.get(workflow) ?? 0) >= 100) {
        operation.performance.clearMeasures(measureName)
        counts.set(workflow, 0)
      }
      operation.performance.measure(measureName, `${measureName}:start`)
      counts.set(workflow, (counts.get(workflow) ?? 0) + 1)
      operation.performance.clearMarks(`${measureName}:start`)
      active.delete(workflow)
    }, 0)
  })
  const cancel = () => {
    cancelAnimationFrame(frame)
    window.clearTimeout(task)
    operation.cancelPaint = undefined
  }
  operation.cancelPaint = cancel
  return cancel
}
