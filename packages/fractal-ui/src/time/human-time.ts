/**
 * The one formatter for human-facing durations and dates (visual-language E41, C7, C19).
 * Pure functions: callers pass `now` so labels are testable with a fixed clock. Locale and
 * time zone default to the viewer's; tests pin both.
 */

export interface HumanTimeZone {
  /** BCP 47 locale for clock and date text; default: the viewer's locale. */
  readonly locale?: string | undefined
  /** IANA time zone for calendar days and clock text; default: the viewer's zone. */
  readonly timeZone?: string | undefined
}

const SECOND = 1_000
const MINUTE = 60 * SECOND
const HOUR = 60 * MINUTE
const DAY = 24 * HOUR
const units = [['d', DAY], ['h', HOUR], ['m', MINUTE], ['s', SECOND]] as const

/**
 * A duration as its largest unit plus the next unit when non-zero: `45s`, `2m`, `2m 5s`,
 * `1h 5m`, `3d`, `3d 4h`. Sub-second and negative spans read `0s`; non-finite input reads `—`.
 * `maxUnits: 1` keeps only the largest unit for compact columns.
 */
export const formatDuration = (milliseconds: number, { maxUnits = 2 }: { readonly maxUnits?: 1 | 2 } = {}): string => {
  if (!Number.isFinite(milliseconds)) return '—'
  let rest = Math.floor(Math.max(0, milliseconds) / SECOND) * SECOND
  const first = units.findIndex(([, size]) => rest >= size)
  if (first === -1) return '0s'
  const parts: string[] = []
  for (const [unit, size] of units.slice(first, first + maxUnits)) {
    const count = Math.floor(rest / size)
    rest -= count * size
    if (count > 0) parts.push(`${count}${unit}`)
  }
  return parts.join(' ')
}

const formatters = new Map<string, Intl.DateTimeFormat>()
const formatter = (zone: HumanTimeZone, kind: string, options: Intl.DateTimeFormatOptions): Intl.DateTimeFormat => {
  const key = `${zone.locale ?? ''}|${zone.timeZone ?? ''}|${kind}`
  let cached = formatters.get(key)
  if (cached === undefined) {
    cached = new Intl.DateTimeFormat(zone.locale, { ...options, timeZone: zone.timeZone })
    formatters.set(key, cached)
  }
  return cached
}

/** Calendar day number of `at` in the zone; differences are DST-safe whole days. */
const calendarDay = (at: number, zone: HumanTimeZone): number => {
  const parts = formatter(zone, 'day', { year: 'numeric', month: 'numeric', day: 'numeric', calendar: 'gregory', numberingSystem: 'latn' }).formatToParts(at)
  const part = (type: Intl.DateTimeFormatPartTypes) => Number(parts.find(entry => entry.type === type)?.value)
  return Math.round(Date.UTC(part('year'), part('month') - 1, part('day')) / DAY)
}
const year = (at: number, zone: HumanTimeZone): string =>
  formatter(zone, 'year', { year: 'numeric', calendar: 'gregory', numberingSystem: 'latn' }).format(at)

/** Local clock time, e.g. `14:02` (en-GB) or `2:02 PM` (en-US). */
export const formatClockTime = (at: number, zone: HumanTimeZone = {}): string =>
  Number.isFinite(at) ? formatter(zone, 'clock', { hour: 'numeric', minute: '2-digit' }).format(at) : '—'

/** A calendar date without time: `6 Oct`, or `6 Oct 2025` outside the current year. */
export const formatDate = (at: number, now: number, zone: HumanTimeZone = {}): string => {
  if (!Number.isFinite(at) || !Number.isFinite(now)) return '—'
  return year(at, zone) === year(now, zone)
    ? formatter(zone, 'date', { day: 'numeric', month: 'short' }).format(at)
    : formatter(zone, 'date-year', { day: 'numeric', month: 'short', year: 'numeric' }).format(at)
}

/**
 * A civil day: `today`, `yesterday`, weekday within six days, then the calendar date.
 * For date-only `YYYY-MM-DD` input, use `civilDate` with the same viewer zone.
 */
export const formatDay = (at: number, now: number, zone: HumanTimeZone = {}): string => {
  if (!Number.isFinite(at) || !Number.isFinite(now)) return '—'
  const days = calendarDay(now, zone) - calendarDay(at, zone)
  if (days === 0) return 'today'
  if (days === 1) return 'yesterday'
  if (days > 1 && days < 7) return formatter(zone, 'weekday', { weekday: 'short' }).format(at)
  return formatDate(at, now, zone)
}

/** Exact civil date only; date-only sources must never imply a reported time of day. */
export const formatExactDay = (at: number, zone: HumanTimeZone = {}): string =>
  Number.isFinite(at)
    ? formatter(zone, 'exact-day', { weekday: 'short', day: 'numeric', month: 'short', year: 'numeric' }).format(at)
    : 'Date unreported'

/** Machine-readable civil date in the formatting zone, never a fabricated UTC instant. */
export const formatDayValue = (at: number, zone: HumanTimeZone = {}): string =>
  Number.isFinite(at) ? new Date(calendarDay(at, zone) * DAY).toISOString().slice(0, 10) : '—'

const weekdayClock = (at: number, zone: HumanTimeZone): string =>
  `${formatter(zone, 'weekday', { weekday: 'short' }).format(at)} ${formatClockTime(at, zone)}`

/**
 * Relative age for attention surfaces: `just now`, `5m ago`, `3h ago` (same day),
 * `yesterday 14:02`, `Mon 14:02` (within a week), then the absolute date.
 * Future instants read `in 5m`, `in 3h`, `in 2d 4h`, then the absolute date.
 */
export const formatRelative = (at: number, now: number, zone: HumanTimeZone = {}): string => {
  if (!Number.isFinite(at) || !Number.isFinite(now)) return '—'
  const delta = now - at
  if (delta < -30 * SECOND) return -delta < 7 * DAY ? `in ${formatDuration(-delta + SECOND - 1, { maxUnits: -delta < DAY ? 1 : 2 })}` : formatDate(at, now, zone)
  if (delta < MINUTE) return 'just now'
  if (delta < HOUR) return `${Math.floor(delta / MINUTE)}m ago`
  const days = calendarDay(now, zone) - calendarDay(at, zone)
  if (days === 0) return `${Math.floor(delta / HOUR)}h ago`
  if (days === 1) return `yesterday ${formatClockTime(at, zone)}`
  if (days < 7) return weekdayClock(at, zone)
  return formatDate(at, now, zone)
}

/** Header time for input cards (E41): `14:02` today, `Mon 14:02` this week, else `6 Oct`. */
export const formatClock = (at: number, now: number, zone: HumanTimeZone = {}): string => {
  if (!Number.isFinite(at) || !Number.isFinite(now)) return '—'
  const days = calendarDay(now, zone) - calendarDay(at, zone)
  return days === 0 ? formatClockTime(at, zone) : days > 0 && days < 7 ? weekdayClock(at, zone) : formatDate(at, now, zone)
}

/** The first instant after `now` that lies a whole number of `unit`s from `anchor`. */
const nextStep = (anchor: number, now: number, unit: number): number => anchor + (Math.floor((now - anchor) / unit) + 1) * unit
/** The smallest unit formatDuration shows; sub-second spans show seconds. */
const smallestShown = (span: number, maxUnits: 1 | 2): number => {
  const first = units.findIndex(([, size]) => span >= size)
  return first === -1 ? SECOND : units[Math.min(units.length - 1, first + maxUnits - 1)]?.[1] ?? SECOND
}

/** Civil-day labels wake at quarter hours, covering modern zones' midnight boundaries. */
export const nextCivilChange = (now: number): number | undefined =>
  Number.isFinite(now) ? nextStep(0, now, 15 * MINUTE) : undefined

/** The next step of the smallest unit formatDuration(now - at) displays. */
export const nextDurationChange = (at: number, now: number, { maxUnits = 2 }: { readonly maxUnits?: 1 | 2 } = {}): number | undefined => {
  if (!Number.isFinite(at) || !Number.isFinite(now)) return undefined
  const elapsed = now - at
  if (elapsed < SECOND) return at + SECOND
  return nextStep(at, now, smallestShown(elapsed, maxUnits))
}

/** The next step of a countdown; undefined once its target has passed. */
export const nextCountdownChange = (at: number, now: number, { maxUnits = 2 }: { readonly maxUnits?: 1 | 2 } = {}): number | undefined => {
  if (!Number.isFinite(at) || !Number.isFinite(now) || at <= now) return undefined
  const remaining = at - now
  const shown = smallestShown(remaining, maxUnits)
  return at - Math.floor(remaining / shown) * shown + 1
}

/** The earliest candidate strictly after now. */
export const earliestChange = (now: number, candidates: Iterable<number | undefined>): number | undefined => {
  let next: number | undefined
  for (const at of candidates) if (at !== undefined && Number.isFinite(at) && at > now && (next === undefined || at < next)) next = at
  return next
}

/** The next relative-label step; minute-granular text wakes once a minute. */
export const nextRelativeChange = (at: number, now: number): number | undefined => {
  if (!Number.isFinite(at) || !Number.isFinite(now)) return undefined
  const delta = now - at
  if (delta < -30 * SECOND) {
    const ahead = -delta
    if (ahead >= 7 * DAY) return Math.min(at - 7 * DAY + 1, nextStep(0, now, 15 * MINUTE))
    const shown = smallestShown(Math.floor((ahead + SECOND - 1) / SECOND) * SECOND, ahead < DAY ? 1 : 2)
    return nextStep(at + SECOND, now, shown)
  }
  if (delta < MINUTE) return at + MINUTE
  if (delta < HOUR) return nextStep(at, now, MINUTE)
  return Math.min(nextStep(at, now, HOUR), nextStep(0, now, 15 * MINUTE))
}

/** The exact local timestamp with zone, for tooltips and accessible descriptions. */
export const formatExact = (at: number, zone: HumanTimeZone = {}): string =>
  Number.isFinite(at)
    ? formatter(zone, 'exact', { weekday: 'short', day: 'numeric', month: 'short', year: 'numeric', hour: 'numeric', minute: '2-digit', second: '2-digit', timeZoneName: 'short' }).format(at)
    : 'Time unreported'

/** Parse an instant from epoch milliseconds or an ISO string; `undefined` when unusable. */
export const instant = (value: number | string | undefined | null): number | undefined => {
  const at = typeof value === 'number' ? value : typeof value === 'string' ? Date.parse(value) : Number.NaN
  return Number.isFinite(at) ? at : undefined
}

/**
 * A YYYY-MM-DD civil date anchored inside that day in the viewer's zone, not UTC midnight.
 * This representative instant is only for day formatters / kind="day"; it reports no clock time.
 * Missing, invalid, or skipped civil dates return undefined.
 */
export const civilDate = (value: string, zone: HumanTimeZone = {}): number | undefined => {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(value)) return undefined
  const noon = Date.parse(`${value}T12:00:00Z`)
  if (!Number.isFinite(noon) || new Date(noon).toISOString().slice(0, 10) !== value) return undefined
  const requestedDay = Math.floor(noon / DAY)
  // Search civil-day boundaries rather than shifting by 24h: the zone's offset can
  // change between the UTC anchor and the requested day (including skipped days).
  let low = noon - 2 * DAY
  let high = noon + 2 * DAY
  while (low <= high) {
    const at = low + Math.floor((high - low) / 2)
    const actualDay = calendarDay(at, zone)
    if (actualDay === requestedDay) return at
    if (actualDay < requestedDay) low = at + 1
    else high = at - 1
  }
  return undefined
}
