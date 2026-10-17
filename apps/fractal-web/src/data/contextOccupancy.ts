import type { UsageSummary } from '@smalltalk/st3-client/schema'
import * as DateTime from 'effect/DateTime'
import type { Known } from './source.ts'

/** Latest reported main-context occupancy, independent of cumulative token/spend accounting. */
export type ContextOccupancy = {
  readonly _tag: 'Known'
  readonly usedTokens?: number
  readonly windowTokens?: number
  readonly usedPercent: number
  readonly observedAt: number
} | { readonly _tag: 'Unknown' }

/** The native UsageContext is exclusively folded from semantics=context_occupancy. */
export const contextOccupancy = ({ usage, now, observedAfter }: {
  readonly usage: Known<UsageSummary>
  readonly now: number
  /** Incarnation transition fence: only a newer occupancy observation may restore the claim. */
  readonly observedAfter?: number
}): ContextOccupancy => {
  const context = usage._tag === 'Known' ? usage.value.context : undefined
  if (context === undefined) return { _tag: 'Unknown' }
  const observedAt = DateTime.toEpochMillis(context.observed_at_unix_ms)
  if (!Number.isFinite(now) || !Number.isFinite(observedAt) || observedAt < 0 || observedAt > now ||
    (observedAfter !== undefined && observedAt <= observedAfter)) return { _tag: 'Unknown' }
  const usedTokens = context.used_tokens
  const windowTokens = context.window_tokens
  const reportedPercent = context.used_percent
  if ([usedTokens, windowTokens, reportedPercent].some(value => value !== undefined && (!Number.isFinite(value) || value < 0))) return { _tag: 'Unknown' }
  const usedPercent = usedTokens !== undefined && windowTokens !== undefined && windowTokens > 0
    ? usedTokens / windowTokens * 100 : reportedPercent
  if (usedPercent === undefined || !Number.isFinite(usedPercent)) return { _tag: 'Unknown' }
  return { _tag: 'Known', usedPercent, observedAt,
    ...(usedTokens === undefined ? {} : { usedTokens }),
    ...(windowTokens === undefined ? {} : { windowTokens }),
  }
}
