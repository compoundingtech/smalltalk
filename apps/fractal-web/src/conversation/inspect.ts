import type { InspectPreview } from './model.ts'

const sensitiveKey = /(token|secret|password|passwd|authorization|api[-_]?key|credential|cookie|session[-_]?key|private[-_]?key)/i
const token = /(?:bearer\s+\S+|\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+|\b(?:sk-|ghp_|xox[baprs]-)[A-Za-z0-9_-]+)/gi
const encoder = new TextEncoder()
const cap = 4096

/** The only raw-record-to-inspector boundary; reading-flow items never carry the raw value. */
export const inspectPreview = (raw: unknown): InspectPreview => {
  let truncated = false
  const seen = new WeakSet<object>()
  const redact = (value: unknown, depth: number): unknown => {
    if (typeof value === 'string') {
      const safe = value.replace(token, '[redacted]')
      if (safe.length <= 400) return safe
      truncated = true
      return `${safe.slice(0, 400)}…`
    }
    if (value === null || typeof value !== 'object') return typeof value === 'bigint' ? String(value) : value
    if (depth >= 6 || seen.has(value)) { truncated = true; return '[truncated]' }
    seen.add(value)
    if (Array.isArray(value)) return value.map(item => redact(item, depth + 1))
    return Object.fromEntries(Object.entries(value).map(([key, item]) =>
      [key, sensitiveKey.test(key) ? '[redacted]' : redact(item, depth + 1)]))
  }
  let full: string
  try { full = JSON.stringify(raw) ?? 'null' } catch { full = '[unserializable record]' }
  let text = JSON.stringify(redact(raw, 0), null, 2) ?? 'null'
  if (encoder.encode(text).length > cap) {
    truncated = true
    // Slice UTF-8, not UTF-16, so multibyte records obey the same byte cap.
    text = new TextDecoder().decode(encoder.encode(text).slice(0, cap - 3)) + '…'
    while (encoder.encode(text).length > cap) text = text.slice(0, -1)
  }
  return { text, bytes: encoder.encode(full).length, truncated }
}
