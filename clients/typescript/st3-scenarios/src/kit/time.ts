/**
 * Time model (spec "Time (D5)"): committed files are written at `ANCHOR`; readers shift exactly the
 * listed instants by `now - anchor`. Offsets (`at_ms`, `started_at_ms`, cast times) never move.
 */
export const ANCHOR = '2030-01-01T12:00:00.000Z'
export const ANCHOR_MS = Date.UTC(2030, 0, 1, 12)

export type TimeCodec = 'timestamp' | 'epoch-ms'
export interface TimePointer {
  readonly pointer: string
  readonly codec: TimeCodec
}

const MIN_MS = -62135596800000 // 0001-01-01T00:00:00.000Z
const MAX_MS = 253402300799999 // 9999-12-31T23:59:59.999Z

export class TimeRangeError extends Error {
  readonly pointer: string
  constructor(pointer: string, detail: string) {
    super(`instant at ${pointer} ${detail}`)
    this.pointer = pointer
  }
}

const RFC3339 = /^(\d{4})-(\d{2})-(\d{2})[Tt](\d{2}):(\d{2}):(\d{2})(?:\.(\d+))?([Zz]|([+-])(\d{2}):(\d{2}))$/

/** Epoch ms of an RFC 3339 instant, truncated (not rounded) to milliseconds. */
export const parseTimestamp = (text: string): number => {
  const m = RFC3339.exec(text)
  if (m === null) throw new Error(`not an RFC 3339 instant: ${JSON.stringify(text)}`)
  const [, y, mo, d, h, mi, s, frac = '', zone, sign, oh, om] = m
  const date = new Date(0)
  date.setUTCFullYear(Number(y), Number(mo) - 1, Number(d))
  date.setUTCHours(Number(h), Number(mi), Number(s), Number(frac.slice(0, 3).padEnd(3, '0')))
  const offset = zone === 'Z' || zone === 'z' ? 0 : (sign === '-' ? -1 : 1) * (Number(oh) * 60 + Number(om)) * 60_000
  return date.getTime() - offset
}

/** Canonical form: UTC, exactly three fractional digits. */
export const formatTimestamp = (ms: number): string => {
  if (!Number.isInteger(ms) || ms < MIN_MS || ms > MAX_MS) throw new RangeError(`instant ${ms} outside 0001..9999`)
  return new Date(ms).toISOString()
}

/** Normalizes a clock reading to integer epoch milliseconds. */
export const nowMs = (now: number | Date | string): number =>
  typeof now === 'number' ? Math.floor(now) : typeof now === 'string' ? parseTimestamp(now) : now.getTime()

/** A clock for generators: every instant is an offset from `now`. */
export interface TimeContext {
  readonly now: number
  /** RFC 3339 instant `offsetMs` from `now`. */
  readonly at: (offsetMs: number) => string
  /** Epoch ms `offsetMs` from `now`. */
  readonly ms: (offsetMs: number) => number
}

export const timeContext = (now: number = ANCHOR_MS): TimeContext => ({
  now,
  at: (offsetMs) => formatTimestamp(now + offsetMs),
  ms: (offsetMs) => now + offsetMs,
})

const unescape = (token: string) => token.replace(/~1/g, '/').replace(/~0/g, '~')
export const escapePointerToken = (token: string) => token.replace(/~/g, '~0').replace(/\//g, '~1')

const tokens = (pointer: string): string[] => {
  if (pointer === '') return []
  if (!pointer.startsWith('/')) throw new Error(`invalid JSON pointer ${pointer}`)
  return pointer.slice(1).split('/').map(unescape)
}

export const getPointer = (doc: unknown, pointer: string): unknown => {
  let value: any = doc
  for (const token of tokens(pointer)) {
    if (value === null || typeof value !== 'object' || !(token in value)) return undefined
    value = value[token]
  }
  return value
}

const setPointer = (doc: any, pointer: string, next: unknown): void => {
  const path = tokens(pointer)
  const last = path.pop()
  if (last === undefined) throw new Error('cannot replace the document root')
  let value = doc
  for (const token of path) value = value[token]
  value[last] = next
}

/**
 * Shifts exactly the listed instants of `doc` from `anchor` to `now`; returns a copy. Inputs are
 * truncated to milliseconds first, so differences are exact relative to the truncated instants.
 */
export const rebase = <A>(doc: A, times: readonly TimePointer[], anchor: string, now: number): A => {
  const delta = nowMs(now) - parseTimestamp(anchor)
  const out = structuredClone(doc)
  for (const { pointer, codec } of times) {
    const value = getPointer(out, pointer)
    if (value === null) continue
    if (codec === 'timestamp') {
      if (typeof value !== 'string') throw new Error(`instant at ${pointer} is not a string`)
      const shifted = parseTimestamp(value) + delta
      if (shifted < MIN_MS || shifted > MAX_MS) throw new TimeRangeError(pointer, 'leaves 0001..9999')
      setPointer(out, pointer, formatTimestamp(shifted))
    } else {
      if (typeof value !== 'number' || !Number.isInteger(value)) throw new Error(`instant at ${pointer} is not an integer`)
      const shifted = value + delta
      if (!Number.isSafeInteger(shifted) || shifted < MIN_MS || shifted > MAX_MS) {
        throw new TimeRangeError(pointer, 'leaves the supported epoch-ms range')
      }
      setPointer(out, pointer, shifted)
    }
  }
  return out
}
