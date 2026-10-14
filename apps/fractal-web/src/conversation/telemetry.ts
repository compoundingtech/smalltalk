import { Effect, Metric } from 'effect'
import type { BlockDegrade } from '@st3/sdk/effect'
import type { ConversationItem } from './model.ts'

// OTLP metrics export is not wired yet: the dev relay forwards only /v1/traces.
export const unsupportedCounter = Metric.counter('fractal.transcript.unsupported', { incremental: true })
export const degradedCounter = Metric.counter('fractal.transcript.degraded', { incremental: true })
const warned = new Set<string>()
export const recordTranscriptTelemetry = (items: readonly ConversationItem[], degraded: readonly BlockDegrade[], issue?: string): void => {
  for (const item of items) {
    if (item._tag !== 'Unsupported') continue
    const attributes = { harness: item.harness, record_kind: item.recordKind, reason: item.reason }
    Effect.runSync(Metric.update(unsupportedCounter.pipe(Metric.withAttributes(attributes)), 1))
    const key = `${item.harness}/${item.recordKind}/${item.reason}`
    if (!warned.has(key)) {
      warned.add(key)
      Effect.runSync(Effect.logWarning('Unsupported transcript record', { ...attributes, issue: issue ?? degraded.find(block => block._tag === 'BlockDropped')?.issue }).pipe(Effect.annotateLogs(attributes)))
    }
  }
  for (const block of degraded) if (block._tag === 'ViewDropped')
    Effect.runSync(Metric.update(degradedCounter.pipe(Metric.withAttributes({ kind: block.kind, view_type: block.viewType })), 1))
}
