/**
 * The monitor's read model: pure projections from decoded usage hub contracts to what the panel
 * shows. Fractal's monitor spec is normative (CAG.CLI.WEB.MON-R01); this file owns bounded presentation only
 * and never reclassifies limits, re-runs pressure selection, or derives ledger identities.
 */
import type { QuotaAccount, QuotaEnvelope, QuotaLimit, UsageHistoryEnvelope } from './wire.ts'

// ── Quota rows ───────────────────────────────────────────────────────────────────────────────

/** One summary cell's textual state; meter and text carry the same fact (CAG.CLI.TUI.TUI.MON-R05). */
export type CellState =
  | { readonly _tag: 'fraction'; readonly percent: number; readonly exhausted: boolean }
  | { readonly _tag: 'exhausted' }
  | { readonly _tag: 'unknown' }
  | { readonly _tag: 'unreported' }

/** One of a row's two summary cells: which limit and its textual state. */
export interface SummaryCell {
  readonly limitId: string
  /** `shortLabel`, else full `label`, else `limitId`; never a duration-derived name. */
  readonly label: string
  readonly state: CellState
}

/** When the anchor limit resets; `unknown` renders as `reset ?`, never a guess. */
export type ResetFact = { readonly _tag: 'at'; readonly iso: string } | { readonly _tag: 'unknown' }

/** Row colour tier derived from pressure: exhausted, ≥90 % critical, ≥75 % warning, else ok. */
export type Severity = 'exhausted' | 'critical' | 'warning' | 'ok' | 'unknown'

/** Account pressure across the complete inventory, including limits hidden behind `+N`. */
export type Pressure =
  | { readonly _tag: 'exhausted' }
  | { readonly _tag: 'known'; readonly fraction: number }
  | { readonly _tag: 'unknown' }

/** A row's two-cell summary, or why the account's evidence cannot be summarized. */
export type RowSummary =
  | {
      readonly _tag: 'summary'
      readonly anchor: SummaryCell
      readonly second: SummaryCell | undefined
      /** No governing declaration: the row shows `pressure-1` + `pressure-2` and says so. */
      readonly declarationGap: boolean
      readonly anchorReset: ResetFact
      /** Limits not shown in either cell; exact, always rendered. */
      readonly omitted: number
    }
  | { readonly _tag: 'incompatible'; readonly reason: string }

/** One account row of the quota table, already sorted and classified. */
export interface QuotaRow {
  /** Canonical account identity; selection keys on this, never on the sorted index. */
  readonly accountId: string
  readonly account: QuotaAccount
  readonly retained: boolean
  readonly summary: RowSummary
  readonly pressure: Pressure
  readonly severity: Severity
}

/** Textual state of one limit's summary cell. */
export const cellState = (limit: QuotaLimit): CellState => {
  if (limit.observation.state === 'unreported') return { _tag: 'unreported' }
  const { usedFraction, exhaustion } = limit.observation.usage
  const exhausted = exhaustion.state === 'exhausted'
  if (usedFraction === undefined || usedFraction === null)
    return exhausted ? { _tag: 'exhausted' } : { _tag: 'unknown' }
  return { _tag: 'fraction', percent: Math.round(usedFraction * 100), exhausted }
}

/** Pressure of one limit: exhausted, a known used fraction, or unknown. */
export const limitPressure = (limit: QuotaLimit): Pressure => {
  if (limit.observation.state === 'unreported') return { _tag: 'unknown' }
  const { usedFraction, exhaustion } = limit.observation.usage
  if (exhaustion.state === 'exhausted') return { _tag: 'exhausted' }
  return usedFraction === undefined || usedFraction === null
    ? { _tag: 'unknown' }
    : { _tag: 'known', fraction: usedFraction }
}

const pressureRank = (pressure: Pressure) =>
  pressure._tag === 'exhausted' ? 2 : pressure._tag === 'known' ? 1 : 0

/** Higher is more pressing: exhausted > highest known fraction > unknown. Positive when `left` is more pressing. */
export const comparePressure = ({
  left,
  right,
}: {
  readonly left: Pressure
  readonly right: Pressure
}): number => {
  if (pressureRank(left) !== pressureRank(right)) return pressureRank(left) - pressureRank(right)
  return left._tag === 'known' && right._tag === 'known' ? left.fraction - right.fraction : 0
}

/** The most pressing limit across the account's complete inventory. */
export const accountPressure = (account: QuotaAccount): Pressure =>
  account.limits
    .map(limitPressure)
    .reduce<Pressure>(
      (max, next) => (comparePressure({ left: next, right: max }) > 0 ? next : max),
      { _tag: 'unknown' },
    )

/** Severity tier for a pressure. */
export const severityOf = (pressure: Pressure): Severity =>
  pressure._tag === 'exhausted'
    ? 'exhausted'
    : pressure._tag === 'unknown'
      ? 'unknown'
      : pressure.fraction >= 0.9
        ? 'critical'
        : pressure.fraction >= 0.75
          ? 'warning'
          : 'ok'

/**
 * The producer's two-cell summary. Governing + `pressure-1` when a governing slot exists, otherwise
 * `pressure-1` + `pressure-2` with the declaration gap. Duplicate slots, a governing role outside the
 * governing slot, or more than two selected limits make the account's evidence incompatible.
 */
export const summarize = (account: QuotaAccount): RowSummary => {
  const bySlot = new Map<string, QuotaLimit>()
  for (const limit of account.limits) {
    if (limit.summaryRole === 'governing' && limit.summarySlot !== 'governing') {
      return {
        _tag: 'incompatible',
        reason: `governing limit ${limit.limitId} sits in slot ${limit.summarySlot}`,
      }
    }
    if (limit.summarySlot === 'none') continue
    if (bySlot.has(limit.summarySlot)) {
      return { _tag: 'incompatible', reason: `duplicate summary slot ${limit.summarySlot}` }
    }
    bySlot.set(limit.summarySlot, limit)
  }
  if (bySlot.size > 2)
    return { _tag: 'incompatible', reason: `${bySlot.size} limits selected for a two-cell summary` }
  const governing = bySlot.get('governing')
  const [anchor, second] =
    governing !== undefined
      ? [governing, bySlot.get('pressure-1')]
      : [bySlot.get('pressure-1'), bySlot.get('pressure-2')]
  if (anchor === undefined) {
    return account.limits.length === 0
      ? { _tag: 'incompatible', reason: 'no limits reported' }
      : { _tag: 'incompatible', reason: 'no summary anchor selected' }
  }
  // `shortLabel`, else full `label`, else `limitId`; never a duration-derived name.
  const cell = (limit: QuotaLimit): SummaryCell => ({
    limitId: limit.limitId,
    label: limit.shortLabel ?? (limit.label === '' ? limit.limitId : limit.label),
    state: cellState(limit),
  })
  return {
    _tag: 'summary',
    anchor: cell(anchor),
    second: second === undefined ? undefined : cell(second),
    declarationGap: governing === undefined,
    anchorReset:
      anchor.observation.state === 'reported' &&
      typeof anchor.observation.window.resetsAt === 'string'
        ? { _tag: 'at', iso: anchor.observation.window.resetsAt }
        : { _tag: 'unknown' },
    omitted: account.limits.length - (second === undefined ? 1 : 2),
  }
}

/** Rows sorted by pressure across the complete inventory, canonical account id ascending on ties. */
export const projectRows = (envelope: QuotaEnvelope): ReadonlyArray<QuotaRow> =>
  envelope.accounts
    .map((account): QuotaRow => {
      const pressure = accountPressure(account)
      return {
        accountId: account.accountId,
        account,
        retained: account.retained,
        summary: summarize(account),
        pressure,
        severity: severityOf(pressure),
      }
    })
    .toSorted(
      (a, b) =>
        comparePressure({ left: b.pressure, right: a.pressure }) ||
        (a.accountId < b.accountId ? -1 : a.accountId > b.accountId ? 1 : 0),
    )

/**
 * Width tiers (CAG.CLI.TUI.TUI.MON-R11): `full` keeps both cells with meters, `compact` drops meters and
 * folds to two lines, `anchor` keeps only the anchor and folds the second limit into `+N`.
 */
export type RowFit = 'full' | 'compact' | 'anchor'

/** Width tier for a panel `px` wide. */
export const fitForWidth = (px: number): RowFit =>
  px >= 560 ? 'full' : px >= 340 ? 'compact' : 'anchor'

/** The `+N` a row shows at `fit`: the anchor-only fallback counts the folded second limit. */
export const omittedAt = ({
  summary,
  fit,
}: {
  readonly summary: Extract<RowSummary, { _tag: 'summary' }>
  readonly fit: RowFit
}) => summary.omitted + (fit === 'anchor' && summary.second !== undefined ? 1 : 0)

// ── Quota dependency state ───────────────────────────────────────────────────────────────────

/** Why a quota or history read failed; unsupported quota relays get a neutral dependency note. */
export type QuotaFailure = {
  readonly kind: 'unavailable' | 'unreachable' | 'http' | 'timeout' | 'incompatible'
  readonly reason: string
}

/** The last successfully decoded quota envelope with its projected rows. */
export interface QuotaSnapshot {
  readonly envelope: QuotaEnvelope
  readonly rows: ReadonlyArray<QuotaRow>
  /** Wall-clock ms the browser received it; distinct from `generatedAt` and `valueObservedAt`. */
  readonly receivedAt: number
}

/** Quota dependency state: undeclared, or declared with the retained snapshot and the latest attempt. */
export type QuotaState =
  | { readonly _tag: 'undeclared' }
  | {
      readonly _tag: 'declared'
      /** The last successfully decoded snapshot; a failed refresh never erases it (CAG.CLI.TUI.TUI.MON-R06). */
      readonly last: QuotaSnapshot | undefined
      readonly attempt:
        | { readonly _tag: 'idle' }
        | { readonly _tag: 'inFlight'; readonly since: number }
        | { readonly _tag: 'ok'; readonly at: number }
        | { readonly _tag: 'failed'; readonly at: number; readonly failure: QuotaFailure }
    }

/** What the poll loop reports to `reduceQuota`. */
export type QuotaEvent =
  | { readonly _tag: 'Requested'; readonly at: number }
  | { readonly _tag: 'Succeeded'; readonly at: number; readonly envelope: QuotaEnvelope }
  | { readonly _tag: 'Failed'; readonly at: number; readonly failure: QuotaFailure }

/** Before the first read; an undeclared source stays undeclared forever. */
export const initialQuota = (declared: boolean): QuotaState =>
  declared
    ? { _tag: 'declared', last: undefined, attempt: { _tag: 'idle' } }
    : { _tag: 'undeclared' }

/** Applies one poll event; a failure keeps the last snapshot (CAG.CLI.TUI.TUI.MON-R06). */
export const reduceQuota = ({
  state,
  event,
}: {
  readonly state: QuotaState
  readonly event: QuotaEvent
}): QuotaState => {
  if (state._tag === 'undeclared') return state
  switch (event._tag) {
    case 'Requested':
      return { ...state, attempt: { _tag: 'inFlight', since: event.at } }
    case 'Succeeded':
      return {
        _tag: 'declared',
        last: { envelope: event.envelope, rows: projectRows(event.envelope), receivedAt: event.at },
        attempt: { _tag: 'ok', at: event.at },
      }
    case 'Failed':
      return { ...state, attempt: { _tag: 'failed', at: event.at, failure: event.failure } }
  }
}

/** The body's last line: one bounded dependency explanation, never an empty or healthy fabrication. */
export type DependencyNote =
  | { readonly _tag: 'undeclared' }
  | { readonly _tag: 'connecting' }
  | { readonly _tag: 'unavailable'; readonly generatedAt: string | undefined }
  | { readonly _tag: 'failed'; readonly failure: QuotaFailure }
  | { readonly _tag: 'current'; readonly generatedAt: string }
  | { readonly _tag: 'stale'; readonly generatedAt: string; readonly horizonSeconds: number }
  | { readonly _tag: 'retained'; readonly generatedAt: string; readonly failure: QuotaFailure }

/** The dependency note for `state` as of `now` (epoch ms). */
export const dependencyNote = ({
  state,
  now,
}: {
  readonly state: QuotaState
  readonly now: number
}): DependencyNote => {
  if (state._tag === 'undeclared') return { _tag: 'undeclared' }
  const { last, attempt } = state
  if (attempt._tag === 'failed' && attempt.failure.kind === 'unavailable')
    return { _tag: 'unavailable', generatedAt: last?.envelope.generatedAt }
  if (last === undefined)
    return attempt._tag === 'failed'
      ? { _tag: 'failed', failure: attempt.failure }
      : { _tag: 'connecting' }
  const generatedAt = last.envelope.generatedAt
  if (attempt._tag === 'failed') return { _tag: 'retained', generatedAt, failure: attempt.failure }
  const ageSeconds = (now - Date.parse(generatedAt)) / 1000
  return ageSeconds > last.envelope.freshnessHorizonSeconds
    ? { _tag: 'stale', generatedAt, horizonSeconds: last.envelope.freshnessHorizonSeconds }
    : { _tag: 'current', generatedAt }
}

// ── Selected-account detail ──────────────────────────────────────────────────────────────────

/** Banked reset credits of one account, as the detail renders them. */
export interface BankedResets {
  readonly count: number
  /** Known exact expiries, soonest first. */
  readonly expiries: ReadonlyArray<{ readonly iso: string; readonly status: string | undefined }>
  /** `availableCount` minus dated credits: expiries the producer did not date. */
  readonly undated: number
}

/** `undefined` when the block is absent: unknown renders nothing, never "0". */
export const bankedResets = (account: QuotaAccount): BankedResets | undefined => {
  const credits = account.resetCredits
  if (credits === undefined || credits === null) return undefined
  const dated = credits.credits
    .flatMap((credit) =>
      typeof credit.expiresAt === 'string'
        ? [
            {
              iso: credit.expiresAt,
              status:
                credit.status === 'available' || credit.status == null ? undefined : credit.status,
            },
          ]
        : [],
    )
    .toSorted((a, b) => Date.parse(a.iso) - Date.parse(b.iso))
  return {
    count: credits.availableCount,
    expiries: dated,
    undated: Math.max(0, credits.availableCount - dated.length),
  }
}

/** Every present `scope.native` field, including `shared: false`. */
export const nativeScopeFacts = (limit: QuotaLimit): ReadonlyArray<string> => {
  const native = limit.scope.native
  if (native === undefined || native === null) return []
  return Object.entries(native).flatMap(([key, value]) =>
    value === undefined || value === null ? [] : [`${key}=${String(value)}`],
  )
}

// ── Account usage history ────────────────────────────────────────────────────────────────────

/** The history chart's reading unit (CAG.CLI.TUI.TUI.MON-R14). */
export type HistoryUnit = 'usd' | 'tokens'

/** One calendar-day column of the history chart. */
export interface HistoryDay {
  /** `YYYY-MM-DD` in the producer's calendar. */
  readonly day: string
  readonly tokens: number
  /** `undefined` when the producer reported no rate-card value for the day. */
  readonly usd: number | undefined
  /** The open day: still accumulating, never read as a finished value. */
  readonly open: boolean
}

/** An account's projected daily usage series up to the open day. */
export interface HistorySeries {
  readonly days: ReadonlyArray<HistoryDay>
  readonly openDay: string
  readonly coverageOk: boolean
  /** Older day buckets the producer's own cap dropped. */
  readonly omittedDays: number
  readonly computedAt: string
}

/** `projectHistory`'s result: a series, or why the envelope cannot be one. */
export type SeriesResult =
  | { readonly _tag: 'series'; readonly series: HistorySeries }
  | { readonly _tag: 'error'; readonly reason: string }

const DAY_MS = 86_400_000
const dayOf = (rfc3339: string) => rfc3339.slice(0, 10)
const offsetOf = (rfc3339: string) => /([+-]\d{2}):(\d{2})$/u.exec(rfc3339)
const nextDay = (day: string) =>
  new Date(Date.parse(`${day}T00:00:00Z`) + DAY_MS).toISOString().slice(0, 10)

/** `computedAt` read through the newest bucket's offset names the open day; no second calendar. */
const openDayOf = ({
  computedAt,
  newestBucketStart,
}: {
  readonly computedAt: string
  readonly newestBucketStart: string | undefined
}): string => {
  const offset = newestBucketStart === undefined ? null : offsetOf(newestBucketStart)
  if (offset === null) return dayOf(new Date(Date.parse(computedAt)).toISOString())
  const sign = offset[1]!.startsWith('-') ? -1 : 1
  const minutes = sign * (Math.abs(Number(offset[1])) * 60 + Number(offset[2]))
  return new Date(Date.parse(computedAt) + minutes * 60_000).toISOString().slice(0, 10)
}

/**
 * One column per calendar day from the oldest reported day to the open day. A day with no row is
 * zero recorded usage, not a gap. Rows for another account are an error, not an empty series.
 * `usd`/`requests` ride the canonical `input` row only; summing them across token types would
 * over-count the day.
 */
export const projectHistory = ({
  envelope,
  ledgerAccountId,
}: {
  readonly envelope: UsageHistoryEnvelope
  readonly ledgerAccountId: string
}): SeriesResult => {
  const perDay = new Map<string, { tokens: number; usd: number | undefined }>()
  let newest: string | undefined
  let omittedDays = 0
  for (const row of envelope.rows) {
    if (row.bucket !== 'day' || row.group_by !== 'account' || row.group !== ledgerAccountId) {
      return {
        _tag: 'error',
        reason: `producer returned ${row.group_by}/${row.bucket} rows for ${row.group ?? 'no group'}`,
      }
    }
    const day = dayOf(row.bucket_start)
    const entry = perDay.get(day) ?? { tokens: 0, usd: undefined }
    entry.tokens += row.tokens ?? 0
    if (row.token_type === 'input' && typeof row.usd === 'number')
      entry.usd = (entry.usd ?? 0) + row.usd
    perDay.set(day, entry)
    if (newest === undefined || row.bucket_start > newest) newest = row.bucket_start
    omittedDays = Math.max(omittedDays, row.omitted_buckets)
  }
  const openDay = openDayOf({ computedAt: envelope.computed_at, newestBucketStart: newest })
  const reported = [...perDay.keys()].toSorted()
  const first = reported[0] ?? openDay
  const last =
    reported.at(-1) !== undefined && reported.at(-1)! > openDay ? reported.at(-1)! : openDay
  const days: HistoryDay[] = []
  for (let day = first; day <= last; day = nextDay(day)) {
    const entry = perDay.get(day)
    days.push({
      day,
      tokens: entry?.tokens ?? 0,
      usd: entry === undefined ? 0 : entry.usd,
      open: day === openDay,
    })
  }
  return {
    _tag: 'series',
    series: {
      days,
      openDay,
      coverageOk: envelope.coverage_ok,
      omittedDays,
      computedAt: envelope.computed_at,
    },
  }
}

/** Dollars only where the producer's coverage gate admits them; the stated preference survives. */
export const effectiveUnit = ({
  preferred,
  series,
}: {
  readonly preferred: HistoryUnit
  readonly series: HistorySeries
}): { readonly unit: HistoryUnit; readonly belowCoverage: boolean } =>
  preferred === 'usd' && !series.coverageOk
    ? { unit: 'tokens', belowCoverage: true }
    : { unit: preferred, belowCoverage: false }

/** One account's history: the last good series survives failed refreshes. */
export type HistoryState = {
  readonly last: { readonly series: HistorySeries; readonly at: number } | undefined
  readonly attempt:
    | { readonly _tag: 'idle' }
    | { readonly _tag: 'inFlight' }
    | { readonly _tag: 'ok' }
    | { readonly _tag: 'failed'; readonly reason: string }
}

/** Before the first history read. */
export const initialHistory: HistoryState = { last: undefined, attempt: { _tag: 'idle' } }

// ── Formatting ───────────────────────────────────────────────────────────────────────────────

/** Signed compact age: `12s`, `-4m` (future evidence keeps its sign), `3h`, `2d`. */
export const compactAge = ({
  fromMs,
  nowMs,
}: {
  readonly fromMs: number
  readonly nowMs: number
}): string => {
  const seconds = Math.round((nowMs - fromMs) / 1000)
  const sign = seconds < 0 ? '-' : ''
  const abs = Math.abs(seconds)
  if (abs < 60) return `${sign}${abs}s`
  if (abs < 3600) return `${sign}${Math.floor(abs / 60)}m`
  if (abs < 86_400) return `${sign}${Math.floor(abs / 3600)}h`
  return `${sign}${Math.floor(abs / 86_400)}d`
}

/** Reset countdown; future sub-minute resets show seconds, past resets say so. */
export const compactReset = ({
  iso,
  nowMs,
}: {
  readonly iso: string
  readonly nowMs: number
}): string => {
  const seconds = Math.round((Date.parse(iso) - nowMs) / 1000)
  if (seconds <= 0) return 'reset due'
  if (seconds < 60) return `${seconds}s`
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`
  if (seconds < 86_400) return `${Math.floor(seconds / 3600)}h`
  return `${Math.floor(seconds / 86_400)}d`
}

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']

/** `Sep 21 08:00Z`, widened to `2027-01-03 08:00Z` when the year differs from now. */
export const exactInstant = ({
  iso,
  nowMs,
}: {
  readonly iso: string
  readonly nowMs: number
}): string => {
  const date = new Date(Date.parse(iso))
  const hhmm = date.toISOString().slice(11, 16)
  return date.getUTCFullYear() === new Date(nowMs).getUTCFullYear()
    ? `${MONTHS[date.getUTCMonth()]} ${date.getUTCDate()} ${hhmm}Z`
    : `${date.toISOString().slice(0, 10)} ${hhmm}Z`
}

/** A summary cell's text; carries the same fact as its meter. */
export const cellText = (state: CellState): string =>
  state._tag === 'fraction'
    ? `${state.percent}%${state.exhausted ? '!' : ''}`
    : state._tag === 'exhausted'
      ? 'exhausted!'
      : state._tag === 'unknown'
        ? '?'
        : 'unreported'

/** Chart value in `unit`: whole or cent dollars, or k/M/B-scaled tokens. */
export const formatUnitValue = ({
  value,
  unit,
}: {
  readonly value: number
  readonly unit: HistoryUnit
}): string => {
  if (unit === 'usd') return value >= 100 ? `$${value.toFixed(0)}` : `$${value.toFixed(2)}`
  if (value >= 1e9) return `${(value / 1e9).toFixed(1)}B`
  if (value >= 1e6) return `${(value / 1e6).toFixed(1)}M`
  if (value >= 1e3) return `${(value / 1e3).toFixed(1)}k`
  return value.toFixed(0)
}
