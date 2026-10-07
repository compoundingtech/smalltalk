/**
 * Usage hub wire contracts the monitor reads (CAG.CLI.WEB.MON-R01 via CAG.CLI.TUI.TUI.MON-R08, R12).
 *
 * Mirrors Fractal's `io/quota.rs` decoder: schema version 3 only, contract fields required, unknown
 * additive fields ignored. Optional facts accept an explicit `null`, because the live producer
 * serializes absent optionals as `null` (probe of build-host-a, 2026-10-01: `scope.native.*`,
 * `usage7d.usd/tokens`, `latestAttempt.reason`).
 */
import { Result, Schema } from 'effect'

const QUOTA_SCHEMA_VERSION = 3 as const

/** An optional fact: the key may be absent or `null`; both mean "not reported". */
const opt = <S extends Schema.Top>(schema: S) => Schema.optionalKey(Schema.NullOr(schema))

const Iso = Schema.String

const QuotaWindow = Schema.Struct({
  id: Schema.String,
  label: Schema.String,
  kind: Schema.String,
  durationMs: opt(Schema.Finite),
  resetsAt: opt(Iso),
})

const QuotaUsage = Schema.Struct({
  usedFraction: opt(Schema.Finite),
  used: opt(Schema.Finite),
  limit: opt(Schema.Finite),
  remaining: opt(Schema.Finite),
  unit: Schema.String,
  rawStatus: opt(Schema.String),
  exhaustion: Schema.Struct({
    state: Schema.Literals(['exhausted', 'available', 'unknown']),
    basis: Schema.Literals(['provider-status', 'utilization', 'unknown']),
    conflict: Schema.Boolean,
  }),
})

/** One provider limit of an account: its summary placement, scope and latest observation. */
export const QuotaLimit = Schema.Struct({
  limitId: Schema.String,
  label: Schema.String,
  shortLabel: opt(Schema.String),
  summaryRole: Schema.Literals(['governing', 'ordinary', 'unknown']),
  summarySlot: Schema.Literals(['governing', 'pressure-1', 'pressure-2', 'none']),
  scope: Schema.Struct({
    class: Schema.Literals(['account', 'model', 'tier', 'product', 'unknown']),
    values: opt(Schema.Array(Schema.String)),
    native: opt(
      Schema.Struct({
        accountId: opt(Schema.String),
        projectId: opt(Schema.String),
        orgId: opt(Schema.String),
        modelId: opt(Schema.String),
        tier: opt(Schema.String),
        windowId: opt(Schema.String),
        shared: opt(Schema.Boolean),
        sharedGroup: opt(Schema.String),
      }),
    ),
  }),
  order: Schema.Struct({
    source: Schema.Literals(['native', 'plan', 'legacy']),
    position: Schema.Finite,
  }),
  observation: Schema.Union([
    Schema.Struct({ state: Schema.Literal('reported'), window: QuotaWindow, usage: QuotaUsage }),
    Schema.Struct({ state: Schema.Literal('unreported') }),
  ]),
})
/** Decoded `QuotaLimit`. */
export type QuotaLimit = typeof QuotaLimit.Type

/** One provider account of the quota envelope with its complete limit inventory. */
export const QuotaAccount = Schema.Struct({
  provider: Schema.String,
  accountId: Schema.String,
  accountLabel: Schema.String,
  /** Producer ledger identity; absent on native st usage, and never reconstructed. */
  ledgerAccountId: Schema.optionalKey(Schema.NonEmptyString),
  latestAttempt: Schema.Struct({ at: Iso, outcome: Schema.String, reason: opt(Schema.String) }),
  valueObservedAt: Iso,
  retained: Schema.Boolean,
  source: Schema.Struct({ authority: Schema.String, confidence: Schema.String }),
  plan: opt(
    Schema.Struct({ id: Schema.String, displayName: Schema.String, authority: Schema.String }),
  ),
  monthlyPrice: opt(
    Schema.Struct({
      amount: Schema.Finite,
      currency: Schema.String,
      basis: Schema.String,
      source: Schema.String,
    }),
  ),
  limits: Schema.Array(QuotaLimit),
  usage7d: Schema.Struct({
    usd: opt(Schema.Finite),
    tokens: opt(Schema.Finite),
    windowStart: opt(Iso),
    basis: Schema.String,
  }),
  /** Absent means unknown, never zero; `availableCount: 0` is a reported zero. */
  resetCredits: opt(
    Schema.Struct({
      availableCount: Schema.Finite,
      credits: Schema.Array(
        Schema.Struct({ grantedAt: opt(Iso), expiresAt: opt(Iso), status: opt(Schema.String) }),
      ),
    }),
  ),
})
/** Decoded `QuotaAccount`. */
export type QuotaAccount = typeof QuotaAccount.Type

/** Usage hub quota v3: every account the producer covers, with its freshness horizon. */
export const QuotaEnvelope = Schema.Struct({
  schemaVersion: Schema.Literal(QUOTA_SCHEMA_VERSION),
  /** Local adapter provenance; native usage has no ledger account identity. */
  sourceSchema: Schema.optionalKey(Schema.Literal('st3.client.v0/usage.period')),
  generatedAt: Iso,
  freshnessHorizonSeconds: Schema.Finite,
  coverage: Schema.Struct({
    expectedAccounts: Schema.optionalKey(Schema.Finite),
    reportedAccounts: Schema.Finite,
    unreportedAccountIds: Schema.Array(Schema.String),
    disabledAccountIds: Schema.Array(Schema.String),
  }),
  accounts: Schema.Array(QuotaAccount),
}).annotate({ identifier: 'OmpUsageQuotaV3' })
/** Decoded `QuotaEnvelope`. */
export type QuotaEnvelope = typeof QuotaEnvelope.Type

/** Usage hub `usage_over_time` lens: per-day, per-token-type rows for the requested account. */
export const UsageHistoryEnvelope = Schema.Struct({
  lens: Schema.Literal('usage_over_time'),
  window: Schema.String,
  computed_at: Iso,
  /** The producer's attribution gate; absolute dollars render only when it admits them. */
  coverage_ok: Schema.Boolean,
  rows: Schema.Array(
    Schema.Struct({
      group_by: Schema.String,
      group: opt(Schema.String),
      bucket: Schema.String,
      /** RFC3339 carrying the producer's calendar offset; its date part names the day. */
      bucket_start: Iso,
      token_type: Schema.String,
      tokens: opt(Schema.Finite),
      usd: opt(Schema.Finite),
      requests: opt(Schema.Finite),
      omitted_buckets: Schema.Finite,
      partial: Schema.Boolean,
    }),
  ),
}).annotate({ identifier: 'OmpUsageUsageOverTime' })
/** Decoded `UsageHistoryEnvelope`. */
export type UsageHistoryEnvelope = typeof UsageHistoryEnvelope.Type

/** A decoded envelope, or the reason the producer's bytes are incompatible. */
export type Decoded<A> =
  | { readonly _tag: 'ok'; readonly value: A }
  | { readonly _tag: 'incompatible'; readonly reason: string }

/** Additive producer fields are ignored, never an incompatibility. */
const decodeWith = <A>({
  schema,
  input,
  what,
}: {
  readonly schema: Schema.Decoder<A>
  readonly input: unknown
  readonly what: string
}): Decoded<A> => {
  const result = Schema.decodeUnknownResult(schema)(input, { onExcessProperty: 'ignore' })
  return Result.isSuccess(result)
    ? { _tag: 'ok', value: result.success }
    : { _tag: 'incompatible', reason: `incompatible usage hub ${what}: ${result.failure.message}` }
}

const VersionProbe = Schema.Struct({ schemaVersion: Schema.Unknown })

/** Version is checked first so a v2 producer reads as "schema version 2", not as a field error. */
export const decodeQuota = (input: unknown, { nativeProjection = false }: { readonly nativeProjection?: boolean } = {}): Decoded<QuotaEnvelope> => {
  const probe = Schema.decodeUnknownResult(VersionProbe)(input)
  if (Result.isSuccess(probe) && probe.success.schemaVersion !== QUOTA_SCHEMA_VERSION) {
    return {
      _tag: 'incompatible',
      reason: `incompatible usage hub quota schema version ${String(probe.success.schemaVersion)} (expected ${QUOTA_SCHEMA_VERSION})`,
    }
  }
  const decoded = decodeWith({ schema: QuotaEnvelope, input, what: 'quota' })
  if (decoded._tag === 'ok' && !nativeProjection &&
      (decoded.value.coverage.expectedAccounts === undefined ||
       decoded.value.accounts.some((account) => account.ledgerAccountId === undefined))) {
    return { _tag: 'incompatible', reason: 'usage hub quota lacks expected coverage or a ledgerAccountId' }
  }
  return decoded
}

/** Decodes one history read. */
export const decodeHistory = (input: unknown): Decoded<UsageHistoryEnvelope> =>
  decodeWith({ schema: UsageHistoryEnvelope, input, what: 'history' })
