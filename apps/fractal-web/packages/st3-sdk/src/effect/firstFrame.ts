/**
 * Each subscribe attempt opens one bounded, always-sampled span: `st3.follow.subscribe` is a
 * child of the caller's active UX span (`parentSpan`), or `wf.ux.first_frame` is an independent
 * root when there is no parent. Both run from the actual subscribe to the first decoded frame.
 * The subscribe command carries that span's context for downstream trace propagation.
 *
 * Attributes are bounded labels only: the follow kind, the window collection name, the outcome,
 * whether the early-connect subscription answered, and (root only) the subscribe-to-first-frame
 * budget and whether an observed first frame met it. Never ids, refs, or text.
 */
import * as Context from 'effect/Context'
import * as Exit from 'effect/Exit'
import * as Option from 'effect/Option'
import * as Tracer from 'effect/Tracer'

import type { EarlyAdoption } from './early.ts'
import { type TraceContext, traceContextOf } from './trace.ts'

export const FIRST_FRAME_SPAN = 'wf.ux.first_frame'
export const FOLLOW_SUBSCRIBE_SPAN = 'st3.follow.subscribe'
/** `wf.ux.budget_phase` on the root: its bounded lifecycle, not a paint. */
export const FIRST_FRAME_BUDGET_PHASE = 'subscribe-to-first-frame'

export type FirstFrameOutcome = 'observed' | 'failed' | 'dropped' | 'superseded' | 'disposed' | 'timeout'

export interface FirstFrameAttempt {
  /** The context the subscribe command carries. */
  readonly trace: TraceContext
  /** End the attempt once; later calls do nothing. */
  readonly end: (outcome: FirstFrameOutcome) => void
}

export interface FirstFrames {
  readonly begin: (args: {
    readonly kind: 'window' | 'conversation' | 'terminal'
    /** Bounded: the window collection name, otherwise the kind. */
    readonly label: string
    /** The early-connect subscribe answered this attempt: link its context and start at its send. */
    readonly adopted?: EarlyAdoption
  }) => FirstFrameAttempt
}

export const makeFirstFrames = ({
  tracer,
  parentSpan,
  deadlineMs = 30_000,
  now = () => performance.now(),
  timeOrigin = performance.timeOrigin,
}: {
  readonly tracer: Tracer.Tracer
  readonly parentSpan?: () => Tracer.Span | undefined
  readonly deadlineMs?: number
  readonly now?: () => number
  readonly timeOrigin?: number
}): FirstFrames => {
  const nanos = (ms: number) => BigInt(Math.round((timeOrigin + ms) * 1_000_000))
  const open = (name: string, parent: Tracer.AnySpan | undefined, startMs: number, attributes: Readonly<Record<string, unknown>>, links: Array<Tracer.SpanLink> = []) => {
    const span = tracer.span({
      name,
      parent: Option.fromUndefinedOr(parent),
      annotations: Context.empty(),
      links,
      startTime: nanos(startMs),
      kind: 'client',
      root: parent === undefined,
      sampled: true,
    })
    for (const [key, value] of Object.entries(attributes)) span.attribute(key, value)
    return span
  }
  return {
    begin: ({ kind, label, adopted }) => {
      const at = now()
      const attributes = {
        'span.label': label,
        'wf.subscription.kind': kind,
        ...(adopted === undefined ? {} : { 'wf.ux.early': true }),
      }
      const bootstrap = adopted === undefined ? undefined : /^00-([0-9a-f]{32})-([0-9a-f]{16})-([0-9a-f]{2})$/.exec(adopted.traceparent)
      const startMs = adopted?.subscribeSentAt ?? at
      const bootstrapSpan = bootstrap == null ? undefined : Tracer.externalSpan({
        traceId: bootstrap[1]!, spanId: bootstrap[2]!, sampled: bootstrap[3] === '01',
      })
      const parent = parentSpan?.()
      const span = open(
        parent === undefined ? FIRST_FRAME_SPAN : FOLLOW_SUBSCRIBE_SPAN,
        parent,
        startMs,
        parent === undefined
          ? { ...attributes, 'wf.ux.budget_ms': deadlineMs, 'wf.ux.budget_phase': FIRST_FRAME_BUDGET_PHASE }
          : attributes,
        bootstrapSpan === undefined ? [] : [{ span: bootstrapSpan, attributes: {} }],
      )
      let ended = false
      const end = (outcome: FirstFrameOutcome) => {
        if (ended) return
        ended = true
        clearTimeout(timer)
        const endMs = now()
        if (parent === undefined)
          span.attribute('wf.ux.budget_met', outcome === 'observed' && endMs - startMs <= deadlineMs)
        span.attribute('wf.ux.outcome', outcome)
        span.end(nanos(endMs), Exit.void)
      }
      // An adopted subscribe was sent before this attempt, so its deadline starts at that send.
      const timer = setTimeout(() => end('timeout'), Math.max(0, deadlineMs - (at - startMs)))
      return { trace: traceContextOf(span), end }
    },
  }
}
