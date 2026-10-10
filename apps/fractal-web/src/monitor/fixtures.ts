/**
 * Usage hub payloads for stories, projected from the shared fixture world (`src/fixtures/world.ts`):
 * the three fleet accounts (`claude/alpha`, `claude/bravo`, `codex/alpha`) carry the world's plans,
 * window fractions, reset times and billed-agent counts; the remaining providers exist only to cover
 * producer shapes the world does not exercise. Shapes follow the v3 producer: explicit `null`
 * optionals, `percent` / `requests` / `credits` units, rolling / fixed_weekly / fixed_monthly windows,
 * declared-unreported limits, and banked resets with fewer dated credits than the count.
 */
import { Effect } from 'effect'

import { quotaAccounts, worldNow, type WorldQuotaAccount } from '../fixtures/world.ts'
import type { QuotaFailure } from './model.ts'
import type { MonitorSource, UsageSource } from './source.ts'
import type { QuotaAccount, QuotaEnvelope, QuotaLimit, UsageHistoryEnvelope } from './wire.ts'

/** The monitor's clock is the world clock. */
export const FIXTURE_NOW = worldNow
const at = (offsetMs: number) => new Date(FIXTURE_NOW + offsetMs).toISOString()
const MIN = 60_000
const HOUR = 60 * MIN
const DAY = 24 * HOUR

const nullNative = {
  accountId: null,
  projectId: null,
  orgId: null,
  modelId: null,
  tier: null,
  windowId: null,
  shared: null,
  sharedGroup: null,
}

interface LimitSpec {
  readonly id: string
  readonly label: string
  readonly short?: string
  readonly slot: QuotaLimit['summarySlot']
  readonly role?: QuotaLimit['summaryRole']
  readonly kind?: string
  readonly fraction?: number | null
  readonly exhausted?: boolean
  readonly unit?: string
  readonly used?: number
  readonly limit?: number
  readonly rawStatus?: string
  readonly resetsIn?: number | null
  readonly unreported?: boolean
  readonly scope?: QuotaLimit['scope']
}

const limitOf = ({
  spec,
  position,
}: {
  readonly spec: LimitSpec
  readonly position: number
}): QuotaLimit => ({
  limitId: spec.id,
  label: spec.label,
  shortLabel: spec.short ?? null,
  summaryRole: spec.role ?? (spec.slot === 'governing' ? 'governing' : 'ordinary'),
  summarySlot: spec.slot,
  scope: spec.scope ?? { class: 'account', native: { ...nullNative, windowId: spec.id } },
  order: { source: spec.unreported === true ? 'plan' : 'native', position },
  observation:
    spec.unreported === true
      ? { state: 'unreported' }
      : {
          state: 'reported',
          window: {
            id: spec.id,
            label: spec.label,
            kind: spec.kind ?? 'fixed_weekly',
            durationMs: spec.kind === 'rolling' ? 5 * HOUR : null,
            resetsAt: spec.resetsIn === null ? null : at(spec.resetsIn ?? 3 * DAY),
          },
          usage: {
            usedFraction: spec.fraction === undefined ? null : spec.fraction,
            used: spec.used ?? (typeof spec.fraction === 'number' ? spec.fraction * 100 : null),
            limit: spec.limit ?? (spec.unit === undefined || spec.unit === 'percent' ? 100 : null),
            remaining: null,
            unit: spec.unit ?? 'percent',
            rawStatus: spec.rawStatus ?? (spec.exhausted === true ? 'blocked' : 'ok'),
            exhaustion: {
              state:
                spec.exhausted === true
                  ? 'exhausted'
                  : spec.fraction == null
                    ? 'unknown'
                    : 'available',
              basis:
                spec.exhausted === true
                  ? 'provider-status'
                  : spec.fraction == null
                    ? 'unknown'
                    : 'utilization',
              conflict: false,
            },
          },
        },
})

interface AccountSpec {
  /** Overrides the default `harness/name` account id. */
  readonly accountId?: string
  readonly harness: string
  readonly provider: string
  readonly name: string
  readonly label: string
  readonly limits: ReadonlyArray<LimitSpec>
  readonly retained?: { readonly observedAgo: number; readonly reason: string }
  readonly plan?: string
  readonly price?: { readonly amount: number; readonly basis: string }
  readonly resets?: QuotaAccount['resetCredits']
  readonly usage7d?: { readonly usd: number; readonly tokens: number }
}

const accountOf = (spec: AccountSpec): QuotaAccount => ({
  provider: spec.provider,
  accountId: spec.accountId ?? `${spec.harness}/${spec.name}`,
  accountLabel: spec.label,
  ledgerAccountId: `${spec.provider}/${spec.name}`,
  latestAttempt:
    spec.retained === undefined
      ? { at: at(-40_000), outcome: 'determinate', reason: null }
      : { at: at(-40_000), outcome: 'indeterminate', reason: spec.retained.reason },
  valueObservedAt: at(spec.retained === undefined ? -42_000 : -spec.retained.observedAgo),
  retained: spec.retained !== undefined,
  source: { authority: 'omp-native', confidence: 'observed' },
  plan:
    spec.plan === undefined
      ? null
      : { id: spec.plan.toLowerCase(), displayName: spec.plan, authority: 'declared' },
  monthlyPrice:
    spec.price === undefined
      ? null
      : { amount: spec.price.amount, currency: 'USD', basis: spec.price.basis, source: 'registry' },
  limits: spec.limits.map((limitSpec, position) => limitOf({ spec: limitSpec, position })),
  usage7d:
    spec.usage7d === undefined
      ? { usd: null, tokens: null, windowStart: at(-7 * DAY), basis: 'unmatched' }
      : { ...spec.usage7d, windowStart: at(-7 * DAY), basis: 'trailing-7d' },
  ...(spec.resets === undefined ? {} : { resetCredits: spec.resets }),
})

const worldAccount = (id: string): WorldQuotaAccount => {
  const found = quotaAccounts.find((a) => a.id === id)
  if (found === undefined) throw new Error(`world has no quota account ${id}`)
  return found
}
const worldWindow = ({
  account,
  window,
}: {
  readonly account: WorldQuotaAccount
  readonly window: '5h' | '7d'
}) => {
  const found = account.windows.find((w) => w.window === window)
  if (found === undefined) throw new Error(`world account ${account.id} has no ${window} window`)
  return { fraction: found.used, resetsIn: Date.parse(found.resetsAt) - FIXTURE_NOW }
}
/** Account label with the number of world agents whose harness bills it. */
const worldLabel = (account: WorldQuotaAccount) =>
  `${account.label} · ${account.agents.length} agents`

const fleetA = worldAccount('anthropic-max-a')
const fleetB = worldAccount('anthropic-max-b')
const codex = worldAccount('openai-pro')

/** Claude-shaped: rolling 5h pressure, weekly governing, tier-scoped weekly with no fraction. */
const claudeLimits = (account: WorldQuotaAccount): ReadonlyArray<LimitSpec> => {
  const weekly = worldWindow({ account, window: '7d' })
  return [
    {
      id: 'anthropic:5h',
      label: 'Claude 5 Hour',
      short: '5h',
      slot: 'pressure-1',
      kind: 'rolling',
      ...worldWindow({ account, window: '5h' }),
    },
    { id: 'anthropic:7d', label: 'Claude 7 Day', short: '7d', slot: 'governing', ...weekly },
    {
      id: 'anthropic:7d-tier',
      label: 'Claude 7 Day (Frontier)',
      short: 'Fbl',
      slot: 'none',
      fraction: null,
      resetsIn: weekly.resetsIn,
      scope: {
        class: 'tier',
        values: ['frontier'],
        native: { ...nullNative, tier: 'frontier', windowId: '7d' },
      },
    },
  ]
}

/** The producer-shape matrix: one account per provider shape the monitor must render. */
export const accounts = {
  claudeAlpha: accountOf({
    harness: 'claude',
    provider: 'anthropic',
    name: 'alpha',
    label: worldLabel(fleetA),
    plan: fleetA.plan,
    price: { amount: 200, basis: 'invoiced' },
    limits: claudeLimits(fleetA),
    resets: {
      availableCount: 2,
      credits: [
        { grantedAt: at(-9 * DAY), expiresAt: at(21 * DAY + 4 * HOUR), status: 'available' },
      ],
    },
  }),
  claudeBravo: accountOf({
    harness: 'claude',
    provider: 'anthropic',
    name: 'bravo',
    label: worldLabel(fleetB),
    plan: fleetB.plan,
    price: { amount: 200, basis: 'list' },
    limits: claudeLimits(fleetB),
    resets: { availableCount: 0, credits: [] },
  }),
  codexAlpha: accountOf({
    harness: 'codex',
    provider: 'openai-codex',
    name: 'alpha',
    label: worldLabel(codex),
    plan: codex.plan,
    price: { amount: 200, basis: 'invoiced' },
    limits: [
      {
        id: 'codex:5h',
        label: '5 Hour',
        short: '5h',
        slot: 'pressure-1',
        kind: 'rolling',
        ...worldWindow({ account: codex, window: '5h' }),
      },
      {
        id: 'codex:7d',
        label: 'Weekly',
        short: '7d',
        slot: 'governing',
        ...worldWindow({ account: codex, window: '7d' }),
      },
      {
        id: 'codex:base',
        label: 'Base model weekly',
        short: 'Base',
        slot: 'none',
        fraction: 0.42,
        resetsIn: worldWindow({ account: codex, window: '7d' }).resetsIn,
        scope: {
          class: 'model',
          values: ['gpt-base'],
          native: {
            accountId: 'acct-synthetic-1',
            projectId: 'proj-1',
            orgId: 'org-1',
            modelId: 'gpt-base',
            tier: 'priority',
            windowId: '7d',
            shared: false,
            sharedGroup: 'team-a',
          },
        },
      },
      { id: 'codex:spark-5h', label: 'Spark 5 Hour', short: 'Sp5', slot: 'none', unreported: true },
      { id: 'codex:spark-7d', label: 'Spark Weekly', short: 'Sp7', slot: 'none', unreported: true },
    ],
    resets: {
      availableCount: 3,
      credits: [
        { grantedAt: at(-2 * DAY), expiresAt: at(5 * DAY), status: 'available' },
        { grantedAt: at(-20 * DAY), expiresAt: at(40 * 1000), status: 'expiring' },
      ],
    },
  }),
  codexBravo: accountOf({
    harness: 'codex',
    provider: 'openai-codex',
    name: 'bravo',
    label: 'Bravo Plus',
    plan: 'Plus',
    price: { amount: 20, basis: 'list' },
    retained: { observedAgo: 47 * MIN, reason: 'provider returned 503 while reading usage' },
    limits: [
      {
        id: 'codex:7d',
        label: 'Weekly',
        short: '7d',
        slot: 'governing',
        fraction: 0.81,
        resetsIn: 4 * DAY,
      },
      {
        id: 'codex:spark-5h',
        label: 'Spark 5 Hour',
        short: 'Sp5',
        slot: 'pressure-1',
        unreported: true,
      },
    ],
  }),
  copilotMain: accountOf({
    harness: 'copilot',
    provider: 'github-copilot',
    name: 'main',
    label: 'Copilot Business',
    plan: 'Business',
    price: { amount: 19, basis: 'list' },
    limits: [
      {
        id: 'premium-requests',
        label: 'Premium requests',
        short: 'Mon',
        slot: 'governing',
        kind: 'fixed_monthly',
        fraction: 0.27,
        unit: 'requests',
        used: 81,
        limit: 300,
        resetsIn: 30 * DAY,
      },
    ],
    usage7d: { usd: 3.12, tokens: 412_000 },
  }),
  zaiMain: accountOf({
    harness: 'zai',
    provider: 'zai',
    name: 'main',
    label: 'GLM Coding',
    limits: [
      {
        id: 'zai:5h',
        label: '5 Hour credits',
        short: '5h',
        slot: 'pressure-1',
        kind: 'rolling',
        fraction: 0.66,
        unit: 'credits',
        resetsIn: 52 * 1000,
      },
      {
        id: 'zai:1w',
        label: 'Weekly credits',
        short: '1w',
        slot: 'pressure-2',
        fraction: 0.12,
        unit: 'credits',
        resetsIn: 5 * DAY,
      },
    ],
    usage7d: { usd: 0.46, tokens: 309_000 },
  }),
  opencodeMain: accountOf({
    harness: 'opencode-go',
    provider: 'opencode-go',
    name: 'main',
    label: 'OpenCode Go',
    plan: 'Go',
    price: { amount: 10, basis: 'list' },
    limits: [
      {
        id: 'og:5h',
        label: '5 Hour',
        short: '5h',
        slot: 'none',
        kind: 'rolling',
        fraction: 0.03,
        resetsIn: 3 * HOUR,
      },
      {
        id: 'og:7d',
        label: 'Weekly',
        short: '7d',
        slot: 'pressure-1',
        fraction: null,
        exhausted: true,
        rawStatus: 'throttled',
        resetsIn: null,
      },
      {
        id: 'og:mon',
        label: 'Monthly',
        short: 'Mon',
        slot: 'governing',
        kind: 'fixed_monthly',
        fraction: 0.23,
        resetsIn: 19 * DAY,
      },
    ],
    usage7d: { usd: 9.22, tokens: 2_118_000_000 },
  }),
  xaiMain: accountOf({
    harness: 'xai',
    provider: 'xai-oauth',
    name: 'main',
    label: 'Grok Heavy',
    plan: 'SuperGrok',
    price: { amount: 30, basis: 'invoiced' },
    limits: [
      {
        id: 'xai:1w',
        label: 'Weekly',
        short: '1w',
        slot: 'governing',
        fraction: null,
        resetsIn: 3 * DAY,
      },
      { id: 'xai:gb', label: 'Grok Build', short: 'GB', slot: 'pressure-1', unreported: true },
    ],
  }),
} satisfies Record<string, QuotaAccount>

/** A producer whose summary is self-contradictory: two `pressure-1` slots. */
export const incompatibleSummaryAccount = accountOf({
  harness: 'claude',
  provider: 'anthropic',
  name: 'charlie',
  label: 'Charlie (broken summary)',
  limits: [
    { id: 'a', label: 'Five hour', short: '5h', slot: 'pressure-1', fraction: 0.2 },
    { id: 'b', label: 'Weekly', short: '7d', slot: 'pressure-1', fraction: 0.4 },
  ],
})

/** A v3 quota envelope over `list` (default: the full matrix), generated `generatedAgo` ms before fixture now. */
export const quotaEnvelope = ({
  list = Object.values(accounts),
  generatedAgo = 40_000,
}: {
  readonly list?: ReadonlyArray<QuotaAccount>
  readonly generatedAgo?: number
} = {}): QuotaEnvelope => ({
  schemaVersion: 3,
  generatedAt: at(-generatedAgo),
  freshnessHorizonSeconds: 900,
  coverage: {
    expectedAccounts: list.length + 1,
    reportedAccounts: list.length,
    unreportedAccountIds: ['claude/delta'],
    disabledAccountIds: [],
  },
  accounts: list,
})

// ── History ──────────────────────────────────────────────────────────────────────────────────

const TOKEN_TYPES = ['input', 'output', 'cache_read', 'cache_write', 'reasoning'] as const

/** Deterministic pseudo-random walk so stories render identically on every build. */
const wave = ({ seed, index }: { readonly seed: number; readonly index: number }) =>
  0.5 + 0.5 * Math.sin(seed * 1.7 + index * 0.9) * Math.cos(seed * 0.3 + index * 0.37)

/** Shape of one synthetic account history. */
export interface HistorySpec {
  readonly ledgerAccountId: string
  readonly days: number
  readonly seed?: number
  /** Days (counted back from the open day, 0 = open) that recorded nothing. */
  readonly idle?: ReadonlyArray<number>
  readonly coverageOk?: boolean
  readonly omittedBuckets?: number
  readonly scale?: number
}

const berlinDay = (daysAgo: number) =>
  new Date(FIXTURE_NOW - daysAgo * DAY).toISOString().slice(0, 10)

/** A deterministic `usage_over_time` envelope for `spec`; day 0 is the open, partial day. */
export const historyEnvelope = (spec: HistorySpec): UsageHistoryEnvelope => {
  const rows: Array<UsageHistoryEnvelope['rows'][number]> = []
  for (let daysAgo = spec.days - 1; daysAgo >= 0; daysAgo--) {
    if (spec.idle?.includes(daysAgo) === true) continue
    const level =
      wave({ seed: spec.seed ?? 1, index: spec.days - daysAgo }) * (daysAgo === 0 ? 0.45 : 1)
    const tokens = Math.round(level * (spec.scale ?? 40_000_000))
    for (const tokenType of TOKEN_TYPES) {
      const share = tokenType === 'cache_read' ? 0.7 : tokenType === 'input' ? 0.12 : 0.06
      rows.push({
        group_by: 'account',
        group: spec.ledgerAccountId,
        bucket: 'day',
        bucket_start: `${berlinDay(daysAgo)}T00:00:00+02:00`,
        token_type: tokenType,
        tokens: Math.round(tokens * share),
        usd: tokenType === 'input' ? Math.round(level * 6_400) / 100 : null,
        requests: tokenType === 'input' ? Math.round(level * 900) : null,
        omitted_buckets: spec.omittedBuckets ?? 0,
        partial: daysAgo === 0,
      })
    }
  }
  return {
    lens: 'usage_over_time',
    window: 'all',
    computed_at: at(-6 * MIN),
    coverage_ok: spec.coverageOk ?? true,
    rows,
  }
}

// ── Scripted sources ─────────────────────────────────────────────────────────────────────────

/** One scripted read answer; `hang` never answers. */
export type Scripted =
  | { readonly _tag: 'ok'; readonly json: unknown; readonly delayMs?: number }
  | { readonly _tag: 'fail'; readonly failure: QuotaFailure; readonly delayMs?: number }
  | { readonly _tag: 'hang' }

/** The n-th read answers `steps[n]` after its own delay (else the source latency); the last step repeats. */
const scripted = ({
  steps,
  latencyMs,
}: {
  readonly steps: ReadonlyArray<Scripted>
  readonly latencyMs: number
}) => {
  let calls = 0
  return Effect.suspend(() => {
    const step = steps[Math.min(calls, steps.length - 1)]!
    calls += 1
    if (step._tag === 'hang') return Effect.never
    const answer: Effect.Effect<unknown, QuotaFailure> =
      step._tag === 'ok' ? Effect.succeed(step.json) : Effect.fail(step.failure)
    const delay = step.delayMs ?? latencyMs
    return delay === 0 ? answer : Effect.delay(answer, delay)
  })
}

/** Scripted quota and per-account history answers for a fixture usage source. */
export interface FixtureSourceOptions {
  readonly label?: string
  readonly quota: ReadonlyArray<Scripted>
  readonly history?: (ledgerAccountId: string) => ReadonlyArray<Scripted>
  readonly latencyMs?: number
  /** Observes every read so stories can assert which account was asked for. */
  readonly onRead?: (what: string) => void
  readonly cadence?: UsageSource['cadence']
}

/** A declared usage source answering from scripts, for stories. */
export const fixtureSource = (options: FixtureSourceOptions): MonitorSource => {
  const latency = options.latencyMs ?? 0
  const histories = new Map<string, Effect.Effect<unknown, QuotaFailure>>()
  const quota = scripted({ steps: options.quota, latencyMs: latency })
  const source: UsageSource = {
    label: options.label ?? 'fixture gateway',
    ...(options.cadence === undefined ? {} : { cadence: options.cadence }),
    quota: Effect.suspend(() => {
      options.onRead?.('quota')
      return quota
    }),
    history: (ledgerAccountId) => {
      let read = histories.get(ledgerAccountId)
      if (read === undefined) {
        read = scripted({
          steps: options.history?.(ledgerAccountId) ?? [
            { _tag: 'ok', json: historyEnvelope({ ledgerAccountId, days: 30 }) },
          ],
          latencyMs: latency,
        })
        histories.set(ledgerAccountId, read)
      }
      const chosen = read
      return Effect.suspend(() => {
        options.onRead?.(`history ${ledgerAccountId}`)
        return chosen
      })
    },
  }
  return { _tag: 'declared', source }
}

/** The workbench population is exactly the world's three accounts, not the producer-shape matrix. */
const worldAccounts: ReadonlyArray<QuotaAccount> = quotaAccounts.map((entry) =>
  accountOf({
    accountId: entry.id,
    harness: entry.provider === 'anthropic' ? 'claude' : 'codex',
    provider: entry.provider,
    name: entry.id,
    label: worldLabel(entry),
    plan: entry.plan,
    limits: entry.windows.map((window) => ({
      id: `${entry.id}:${window.window}`,
      label: window.window === '5h' ? '5 Hour' : 'Weekly',
      short: window.window,
      slot: window.window === '5h' ? 'pressure-1' : 'governing',
      kind: window.window === '5h' ? 'rolling' : 'fixed_weekly',
      fraction: window.used,
      resetsIn: Date.parse(window.resetsAt) - FIXTURE_NOW,
    })),
  }),
)

/** Shared by the workbench and overview: complete world coverage and successful histories. */
export const worldUsageSource: MonitorSource = fixtureSource({
  label: 'gateway build-host-a',
  quota: [
    {
      _tag: 'ok',
      json: {
        ...quotaEnvelope({ list: worldAccounts }),
        coverage: {
          expectedAccounts: worldAccounts.length,
          reportedAccounts: worldAccounts.length,
          unreportedAccountIds: [],
          disabledAccountIds: [],
        },
      },
    },
  ],
  history: (ledgerAccountId) => [
    {
      _tag: 'ok',
      json: historyEnvelope({ ledgerAccountId, days: 45, seed: ledgerAccountId.length }),
    },
  ],
  cadence: { quota: '30 seconds', history: '1 second' },
})
