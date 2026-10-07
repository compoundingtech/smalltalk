/** Explicit presentation adapter for the canonical st3.client.v0 usage.period projection. */
import { UsagePeriod } from '@smalltalk/st3-client/schema'
import type { UsageLimit } from '@smalltalk/st3-client/schema'
import { Result, Schema } from 'effect'

import type { QuotaFailure } from './model.ts'
import type { QuotaAccount, QuotaEnvelope, QuotaLimit } from './wire.ts'

/** Presentation freshness bound, matching the native default account-limits policy. */
export const NATIVE_LIMIT_FRESHNESS_MS = 60 * 60 * 1000
const NativeEnvelope = Schema.Struct({
  api_version: Schema.Literal('st3.client.v0'),
  value: UsagePeriod,
})

const iso = (ms: number): string | undefined => {
  const date = new Date(ms)
  return Number.isFinite(date.getTime()) ? date.toISOString() : undefined
}

const limit = ({ reading, window, slot, now }: {
  readonly reading: UsageLimit
  readonly window: '5h' | 'weekly'
  readonly slot: 'pressure-1' | 'pressure-2'
  readonly now: number
}): QuotaLimit => {
  const percent = window === '5h' ? reading.five_hour_percent : reading.weekly_percent
  const resetMs = window === '5h' ? reading.five_hour_resets_at_unix_ms : reading.weekly_resets_at_unix_ms
  const fresh = now - reading.measured_at_unix_ms <= NATIVE_LIMIT_FRESHNESS_MS &&
    reading.measured_at_unix_ms <= now &&
    (resetMs === undefined || resetMs > now)
  const known = percent !== undefined && Number.isFinite(percent) && percent >= 0 && fresh
  return {
    limitId: window,
    label: window === '5h' ? '5-hour' : 'Weekly',
    shortLabel: window,
    // st does not declare a governing limit or a ranked summary inventory.
    summaryRole: 'unknown',
    summarySlot: slot,
    scope: { class: 'account' },
    order: { source: 'native', position: slot === 'pressure-1' ? 0 : 1 },
    observation: percent === undefined ? { state: 'unreported' } : {
      state: 'reported',
      window: { id: window, label: window === '5h' ? '5-hour' : 'Weekly', kind: 'provider',
        ...(resetMs === undefined || iso(resetMs) === undefined ? {} : { resetsAt: iso(resetMs) }) },
      usage: {
        unit: 'fraction',
        ...(known ? { usedFraction: percent / 100 } : {}),
        ...(!fresh ? { rawStatus: 'stale, future-dated, or already reset observation' } : {}),
        exhaustion: { state: known ? (percent >= 100 ? 'exhausted' : 'available') : 'unknown',
          basis: known ? 'utilization' : 'unknown', conflict: false },
      },
    },
  }
}

export const projectNativeQuota = (input: unknown, now: number):
  | { readonly _tag: 'ok'; readonly value: QuotaEnvelope }
  | { readonly _tag: 'failed'; readonly failure: QuotaFailure } => {
  const decoded = Schema.decodeUnknownResult(NativeEnvelope)(input)
  if (Result.isFailure(decoded)) return { _tag: 'failed', failure: {
    kind: 'incompatible', reason: `incompatible st usage.period: ${decoded.failure.message}`,
  } }
  const period = decoded.success.value
  if (period.limits === undefined) return { _tag: 'failed', failure: {
    kind: 'unavailable', reason: 'This st usage.period response does not report account limits.',
  } }
  if (period.limits.some((reading) => iso(reading.measured_at_unix_ms) === undefined)) {
    return { _tag: 'failed', failure: { kind: 'incompatible', reason: 'st usage.period observation time is not representable.' } }
  }
  const accounts: QuotaAccount[] = period.limits.map((reading) => {
    const observedAt = new Date(reading.measured_at_unix_ms).toISOString()
    const stale = now - reading.measured_at_unix_ms > NATIVE_LIMIT_FRESHNESS_MS ||
      reading.measured_at_unix_ms > now
    return {
      provider: reading.driver,
      accountId: reading.account_ref ?? reading.account,
      accountLabel: reading.account_ref ?? reading.account,
      // No ledgerAccountId: a declared/native account is not a per-request ledger identity.
      latestAttempt: { at: observedAt, outcome: 'observed',
        ...(stale ? { reason: 'source observation stale or future-dated' } : {}) },
      valueObservedAt: observedAt,
      retained: stale,
      source: { authority: 'st3.usage.period', confidence: reading.identified === false ? 'identity unknown' : 'harness observation' },
      ...(reading.plan === undefined ? {} : { plan: { id: reading.plan, displayName: reading.plan, authority: 'harness observation' } }),
      limits: [limit({ reading, window: 'weekly', slot: 'pressure-1', now }),
        limit({ reading, window: '5h', slot: 'pressure-2', now })],
      // The period's default 24h spend is not seven-day spend or per-account ledger history.
      usage7d: { basis: 'not reported by st usage.period' },
    }
  })
  return { _tag: 'ok', value: {
    schemaVersion: 3,
    sourceSchema: 'st3.client.v0/usage.period',
    generatedAt: new Date(now).toISOString(),
    freshnessHorizonSeconds: 30,
    coverage: { reportedAccounts: accounts.length,
      unreportedAccountIds: [], disabledAccountIds: [] },
    accounts,
  } }
}
