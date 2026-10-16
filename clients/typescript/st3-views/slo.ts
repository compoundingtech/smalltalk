/** Device-local summaries shared by stui and the phone. No resource identity or content. */
export const SLO_BUCKET_UPPER_MS = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, null] as const;
export const SLO_COUNTERS = ['active_ms', 'panics', 'silent_exits', 'launches', 'first_screen_top_bar_flips', 'first_screen_placeholder_flips', 'conversation_opens', 'open_not_bound', 'open_remote_unavailable', 'open_cannot_load', 'drafts_restored_unclean', 'not_live_ms', 'direct_attach_attempts', 'direct_attach_success', 'gateway_attach', 'terminal_open_errors', 'dead_terminal_tabs', 'measurement_overflow'] as const;
export const SLO_HISTOGRAMS = ['first_frame_ms', 'first_real_state_ms', 'conversation_open_ms', 'key_to_redraw_ms', 'freshness_ms', 'reconnect_ms', 'terminal_open_ms', 'alert_landed_ms', 'send_echo_ms', 'send_ack_ms', 'send_visible_ms'] as const;
export type SloCounter = typeof SLO_COUNTERS[number];
export type SloHistogram = typeof SLO_HISTOGRAMS[number];
/** Initial guesses, to revisit after one week; the Rust reference fixture tests this table. */
export const SLO_TARGETS = {
  panics_max: 0,
  silent_exits_per_100_active_hours_lt: 1,
  first_frame_p95_ms_lt: 300,
  first_real_state_p95_ms_lt: 2000,
  conversation_open_p50_ms_lt: 300,
  conversation_open_p95_ms_lt: 1000,
  conversation_error_percent_lt: 0.5,
  key_to_redraw_p95_ms_lt: 50,
  reconnect_p95_ms_lt: 10000,
  direct_attach_success_percent_gte: 99,
  terminal_open_p95_ms_lt: 1000,
  alert_landed_p95_ms_lt: 2000,
  send_echo_p99_ms_lt: 100,
  send_visible_p99_ms_lt: 200,
} as const;
export type SloTarget = keyof typeof SLO_TARGETS;
export type QualityHour = {
  version: 1; window_start: number; host: string; build: string;
  bucket_upper_ms: (number | null)[];
  counters: Record<SloCounter, number>;
  histograms: Record<SloHistogram, number[]>;
  targets: Record<SloTarget, number>;
};
export const SLO_HISTORY_HOURS = 336;
const sum = (a: number, b: number) => Math.min(Number.MAX_SAFE_INTEGER, a + b);
export function sloBucket(ms: number): number {
  if (!Number.isFinite(ms)) return ms === Infinity ? 16 : 0;
  for (let i = 0; i < 16; i++) if (ms <= SLO_BUCKET_UPPER_MS[i]!) return i;
  return 16;
}
/** JS callbacks execute on one thread. Fixed numeric storage, no allocation in add/observe. */
export class SloCounters {
  private readonly counts = new Float64Array(SLO_COUNTERS.length);
  private readonly buckets = new Float64Array(SLO_HISTOGRAMS.length * 17);
  add(name: SloCounter, n = 1): void {
    if (!Number.isFinite(n) || n < 0) return;
    const i = SLO_COUNTERS.indexOf(name);
    this.counts[i] = sum(this.counts[i], Math.floor(n));
  }
  observe(name: SloHistogram, ms: number): void {
    if (Number.isNaN(ms) || ms < 0) return;
    const i = SLO_HISTOGRAMS.indexOf(name) * 17 + sloBucket(ms);
    this.buckets[i] = sum(this.buckets[i], 1);
  }
  /** Serialization and snapshots happen outside input/presentation accounting. */
  line(window: number, host: string, build: string, drain = false): QualityHour {
    const counters = Object.fromEntries(SLO_COUNTERS.map((name, i) => [name, this.counts[i]])) as QualityHour['counters'];
    const histograms = Object.fromEntries(SLO_HISTOGRAMS.map((name, i) => [name, Array.from(this.buckets.subarray(i * 17, (i + 1) * 17))])) as QualityHour['histograms'];
    if (drain) { this.counts.fill(0); this.buckets.fill(0); }
    return { version: 1, window_start: window, host, build, bucket_upper_ms: [...SLO_BUCKET_UPPER_MS], counters, histograms, targets: { ...SLO_TARGETS } };
  }
}
/** Start of the device's local clock hour, as Unix seconds (includes half-hour zones). */
export function sloHour(wallMs: number): number {
  const at = new Date(wallMs);
  // Subtract within this occurrence of the hour; setMinutes would pick the
  // first occurrence of a repeated DST hour and schedule a past boundary.
  return Math.floor(wallMs / 1000) - at.getMinutes() * 60 - at.getSeconds();
}
function exactKeys(value: unknown, keys: readonly string[]): value is Record<string, unknown> {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
  const names = Object.keys(value);
  return names.length === keys.length && names.every(name => keys.includes(name));
}
const integer = (n: unknown): n is number => typeof n === 'number' && Number.isSafeInteger(n) && n >= 0;
export function isQualityHour(value: unknown): value is QualityHour {
  if (!exactKeys(value, ['version', 'window_start', 'host', 'build', 'bucket_upper_ms', 'counters', 'histograms', 'targets'])) return false;
  return value.version === 1 && Number.isSafeInteger(value.window_start)
    && typeof value.host === 'string' && value.host.length <= 256
    && typeof value.build === 'string' && value.build.length <= 256
    && Array.isArray(value.bucket_upper_ms) && value.bucket_upper_ms.length === 17 && value.bucket_upper_ms.every((n, i) => n === SLO_BUCKET_UPPER_MS[i])
    && exactKeys(value.counters, SLO_COUNTERS) && Object.values(value.counters).every(integer)
    && exactKeys(value.histograms, SLO_HISTOGRAMS) && Object.values(value.histograms).every(counts => Array.isArray(counts) && counts.length === 17 && counts.every(integer))
    && exactKeys(value.targets, Object.keys(SLO_TARGETS)) && Object.values(value.targets).every(n => typeof n === 'number' && Number.isFinite(n) && n >= 0);
}
/** Add simultaneous/partial contributions; the latest contributor names the build, the hour keeps its targets. */
export function mergeQualityHour(a: QualityHour, b: QualityHour): QualityHour {
  const counters = Object.fromEntries(SLO_COUNTERS.map(name => [name, sum(a.counters[name], b.counters[name])])) as QualityHour['counters'];
  const histograms = Object.fromEntries(SLO_HISTOGRAMS.map(name => [name, a.histograms[name].map((n, i) => sum(n, b.histograms[name][i]))])) as QualityHour['histograms'];
  return { ...a, build: b.build, counters, histograms };
}
export function retainQualityHours(history: readonly QualityHour[], line: QualityHour): QualityHour[] {
  const latest = Math.max(line.window_start, ...history.map(row => row.window_start));
  const cutoff = latest - (SLO_HISTORY_HOURS - 1) * 3600;
  const rows = history.filter(row => row.window_start >= cutoff);
  if (line.window_start >= cutoff) {
    const i = rows.findIndex(row => row.window_start === line.window_start);
    if (i === -1) rows.push(line); else rows[i] = mergeQualityHour(rows[i], line);
  }
  return rows.sort((a, b) => a.window_start - b.window_start).slice(-SLO_HISTORY_HOURS);
}
export function qualityPercentile(counts: readonly number[], percentile: number): number | null {
  const total = counts.reduce(sum, 0);
  if (!total) return null;
  const rank = Math.max(1, Math.ceil(total * percentile / 100));
  let kept = 0;
  for (let i = 0; i < 17; i++) { kept = sum(kept, counts[i]); if (kept >= rank) return SLO_BUCKET_UPPER_MS[i] ?? Infinity; }
  return Infinity;
}
export type QualityJudgment = { target: SloTarget; value: number | null; limit: number; pass: boolean | null };
export function judgeQuality(hour: QualityHour): QualityJudgment[] {
  const c = hour.counters;
  const p = (hist: SloHistogram, n = 95) => qualityPercentile(hour.histograms[hist], n);
  const values: Record<SloTarget, number | null> = {
    panics_max: c.active_ms || c.panics ? c.panics : null,
    silent_exits_per_100_active_hours_lt: c.active_ms ? c.silent_exits * 360_000_000 / c.active_ms : null,
    first_frame_p95_ms_lt: p('first_frame_ms'), first_real_state_p95_ms_lt: p('first_real_state_ms'),
    conversation_open_p50_ms_lt: p('conversation_open_ms', 50), conversation_open_p95_ms_lt: p('conversation_open_ms'),
    conversation_error_percent_lt: c.conversation_opens ? 100 * (c.open_not_bound + c.open_remote_unavailable + c.open_cannot_load) / c.conversation_opens : null,
    key_to_redraw_p95_ms_lt: p('key_to_redraw_ms'), reconnect_p95_ms_lt: p('reconnect_ms'),
    direct_attach_success_percent_gte: c.direct_attach_attempts ? 100 * c.direct_attach_success / c.direct_attach_attempts : null,
    terminal_open_p95_ms_lt: p('terminal_open_ms'), alert_landed_p95_ms_lt: p('alert_landed_ms'),
    send_echo_p99_ms_lt: p('send_echo_ms', 99), send_visible_p99_ms_lt: p('send_visible_ms', 99),
  };
  return (Object.keys(SLO_TARGETS) as SloTarget[]).map(target => {
    const value = values[target], limit = hour.targets[target];
    return { target, value, limit, pass: value === null ? null : target.endsWith('_max') ? value <= limit : target.endsWith('_gte') ? value >= limit : value < limit };
  });
}
