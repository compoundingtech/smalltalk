// What the phone measures of its own service, kept on the device and reported to the member it is
// connected to, which merges it into the daemon's SLO windows. Nothing here has a timer of its own:
// an interval closes when the next sample or the next report finds its minute over.
//
// The unit is a one-minute interval (UTC, minute aligned). A closed interval is sent once, in a batch
// with a stable report ID; a batch that was not accepted is sent again exactly as it was, never rebuilt
// or summed into a newer one. An interval older than seven days is dropped. A sample holds a latency
// and where it was served from: no message content, address, node ID or credential, ever.

export const TARGETS = {
  'ios-open-to-live': 3000,
  'ios-connect': 1500,
  'ios-message-ack': 500,
  'ios-conversation-open': 1000,
  'ios-terminal-open': 1500,
  'ios-recover': 10_000,
} as const;
export type Target = keyof typeof TARGETS;
export type Carrier = 'fabric' | 'tailscale' | 'lan' | 'https';
export type Context = { carrier: Carrier; path?: 'direct' | 'relay' };

export const MINUTE_MS = 60_000;
export const KEEP_MS = 7 * 24 * 60 * MINUTE_MS;
/** The most intervals one report carries. */
export const MAX_INTERVALS_PER_REPORT = 30;
/** The most closed intervals kept on the device while no member accepts them. */
export const MAX_KEPT_INTERVALS = 1500;
/** The histogram has this many buckets for each doubling, as the daemon's windows do. */
export const BUCKETS_PER_DOUBLING = 16;

type Series = { target: Target; carrier: Carrier; path?: 'direct' | 'relay'; count: number; over_target: number; max_ms: number; buckets: Record<number, number> };
type Share = { carrier: Carrier; path?: 'direct' | 'relay'; foreground_ms: number; live_ms: number };
type Interval = { start: number; series: Record<string, Series>; shares: Record<string, Share> };

export type WireSample = { target: Target; carrier: Carrier; path?: 'direct' | 'relay'; count: number; over_target: number; max_ms: number; buckets: Array<[number, number]> };
export type WireShare = { carrier: Carrier; path?: 'direct' | 'relay'; foreground_ms: number; live_ms: number };
export type WireInterval = { interval_start: string; interval_end: string; samples: WireSample[]; shares: WireShare[] };
export type Batch = { report_id: string; intervals: WireInterval[] };

export type Storage = { read(): Promise<string | null>; write(value: string): Promise<void> };
export type Clock = { now(): number; id(): string };

const floorMinute = (ms: number) => Math.floor(ms / MINUTE_MS) * MINUTE_MS;
const keyOf = (context: Context, target?: string) => `${target ?? 'share'}|${context.carrier}|${context.path ?? ''}`;

/** The histogram bucket of a latency, and a latency to report for a bucket (its upper edge, in whole ms). */
export function bucketOf(ms: number): number { return Math.floor(BUCKETS_PER_DOUBLING * Math.log2(Math.max(1, ms))); }
export function bucketEdge(index: number): number { return Math.ceil(2 ** ((index + 1) / BUCKETS_PER_DOUBLING)); }

function wire(interval: Interval): WireInterval {
  return {
    interval_start: new Date(interval.start).toISOString(),
    interval_end: new Date(interval.start + MINUTE_MS).toISOString(),
    samples: Object.values(interval.series).map(series => ({
      target: series.target, carrier: series.carrier, ...(series.path ? { path: series.path } : {}),
      count: series.count, over_target: series.over_target, max_ms: series.max_ms,
      buckets: Object.entries(series.buckets).map(([index, count]) => [bucketEdge(Number(index)), count] as [number, number]).sort((a, b) => a[0] - b[0]),
    })),
    shares: Object.values(interval.shares).map(share => ({ carrier: share.carrier, ...(share.path ? { path: share.path } : {}), foreground_ms: share.foreground_ms, live_ms: share.live_ms })),
  };
}

type Saved = { closed: Interval[]; minutes?: Interval[]; batch?: { report_id: string; intervals: Interval[] } };

export class Observations {
  // Minutes that are not over yet (usually one; a stretch of foreground time can reach back a few).
  private minutes = new Map<number, Interval>();
  private closed: Interval[] = [];
  private batch: { report_id: string; intervals: Interval[] } | null = null;
  private timers = new Map<number, { target: Target; began: number }>();
  private nextToken = 1;
  // The share of foreground time with a live feed: when each state last changed.
  private inForeground = false;
  private feedLive = false;
  private since = 0;
  private loaded = false;

  constructor(private readonly storage: Storage, private readonly clock: Clock, private readonly context: () => Context) {}

  /** Restore what an earlier run kept. Safe to call once, early; samples before it are kept too. */
  async load(): Promise<void> {
    if (this.loaded) return;
    this.loaded = true;
    try {
      const raw = await this.storage.read();
      const saved = raw ? JSON.parse(raw) as Saved : null;
      if (saved && Array.isArray(saved.closed)) {
        // Minutes an earlier run left open are over by now unless this very minute; the rest join the closed ones.
        for (const interval of (saved.minutes ?? []).filter(valid)) this.add(interval);
        this.closed = [...saved.closed.filter(valid), ...this.closed].sort((a, b) => a.start - b.start);
        if (saved.batch && Array.isArray(saved.batch.intervals) && typeof saved.batch.report_id === 'string' && !this.batch) {
          this.batch = { report_id: saved.batch.report_id, intervals: saved.batch.intervals.filter(valid) };
        }
        this.sweep();
      }
    } catch { /* a damaged record is dropped; measuring goes on */ }
  }

  /** Add a restored not-yet-closed minute, summing into one this run already started in the same minute. */
  private add(restored: Interval): void {
    const present = this.minutes.get(restored.start);
    if (!present) { this.minutes.set(restored.start, restored); return; }
    for (const [key, series] of Object.entries(restored.series)) {
      const into = present.series[key];
      if (!into) { present.series[key] = series; continue; }
      into.count += series.count; into.over_target += series.over_target; into.max_ms = Math.max(into.max_ms, series.max_ms);
      for (const [bucket, count] of Object.entries(series.buckets)) into.buckets[Number(bucket)] = (into.buckets[Number(bucket)] ?? 0) + count;
    }
    for (const [key, share] of Object.entries(restored.shares)) {
      const into = present.shares[key];
      if (!into) present.shares[key] = share; else { into.foreground_ms += share.foreground_ms; into.live_ms += share.live_ms; }
    }
  }

  // ---- Latencies.
  /** Start timing a target; `end(token)` records it. A token is dropped by `cancel`. */
  begin(target: Target): number {
    const token = this.nextToken++;
    this.timers.set(token, { target, began: this.clock.now() });
    // A timer that is never ended must not grow without bound.
    if (this.timers.size > 64) { const oldest = this.timers.keys().next().value; if (oldest !== undefined) this.timers.delete(oldest); }
    return token;
  }
  end(token: number): void {
    const timer = this.timers.get(token);
    if (!timer) return;
    this.timers.delete(token);
    this.observe(timer.target, this.clock.now() - timer.began);
  }
  cancel(token: number): void { this.timers.delete(token); }

  observe(target: Target, ms: number, context: Context = this.context()): void {
    if (!Number.isFinite(ms) || ms < 0) return;
    const interval = this.intervalAt(this.clock.now());
    const key = keyOf(context, target);
    const series = interval.series[key] ??= { target, carrier: context.carrier, ...(context.path ? { path: context.path } : {}), count: 0, over_target: 0, max_ms: 0, buckets: {} };
    const whole = Math.round(ms);
    series.count++;
    if (whole > TARGETS[target]) series.over_target++;
    series.max_ms = Math.max(series.max_ms, whole);
    const bucket = bucketOf(whole);
    series.buckets[bucket] = (series.buckets[bucket] ?? 0) + 1;
  }

  // ---- The share of foreground time with a live feed.
  setForeground(on: boolean): void { this.advanceShare(); this.inForeground = on; }
  setLive(on: boolean): void { this.advanceShare(); this.feedLive = on; }

  /** Account the time since the last change to the minute (or minutes) it spans. */
  private advanceShare(): void {
    const now = this.clock.now();
    if (this.since && this.inForeground && now > this.since) {
      const context = this.context();
      let from = this.since;
      while (from < now) {
        const edge = Math.min(now, floorMinute(from) + MINUTE_MS);
        const interval = this.intervalAt(from);
        const share = interval.shares[keyOf(context)] ??= { carrier: context.carrier, ...(context.path ? { path: context.path } : {}), foreground_ms: 0, live_ms: 0 };
        share.foreground_ms += edge - from;
        if (this.feedLive) share.live_ms += edge - from;
        from = edge;
      }
    }
    this.since = now;
  }

  // ---- Intervals.
  private intervalAt(at: number): Interval {
    const start = floorMinute(at);
    let interval = this.minutes.get(start);
    if (!interval) {
      interval = { start, series: {}, shares: {} };
      this.minutes.set(start, interval);
      // A new minute means earlier ones are over.
      if (this.minutes.size > 1) this.sweep();
    }
    return interval;
  }

  /** Minutes that are over become closed intervals; nothing is dropped but what is empty or older than a week. */
  private sweep(): void {
    const current = floorMinute(this.clock.now());
    let moved = false;
    for (const [start, interval] of [...this.minutes]) {
      if (start >= current) continue;
      this.minutes.delete(start);
      if (Object.keys(interval.series).length || Object.keys(interval.shares).length) { this.closed.push(interval); moved = true; }
    }
    if (moved) this.closed.sort((a, b) => a.start - b.start);
    const floor = this.clock.now() - KEEP_MS;
    this.closed = this.closed.filter(interval => interval.start + MINUTE_MS > floor);
    if (this.closed.length > MAX_KEPT_INTERVALS) this.closed = this.closed.slice(-MAX_KEPT_INTERVALS);
    if (this.batch) {
      this.batch.intervals = this.batch.intervals.filter(interval => interval.start + MINUTE_MS > floor);
      if (!this.batch.intervals.length) this.batch = null;
    }
    if (moved) void this.persist();
  }

  // ---- Reporting.
  /**
   * The report to send now, or null when nothing is waiting. A batch is formed once from the closed
   * intervals and kept, with its ID, until a member accepts it, so every retry sends identical bytes.
   * The minute still open is not in it.
   */
  nextReport(): Batch | null {
    this.advanceShare();
    this.sweep();
    if (!this.batch) {
      if (!this.closed.length) return null;
      const intervals = this.closed.splice(0, MAX_INTERVALS_PER_REPORT);
      this.batch = { report_id: this.clock.id(), intervals };
      void this.persist();
    }
    return { report_id: this.batch.report_id, intervals: this.batch.intervals.map(wire) };
  }

  /** A member accepted the report: its intervals are done. */
  accepted(reportId: string): void {
    if (this.batch?.report_id !== reportId) return;
    this.batch = null;
    void this.persist();
  }

  /** Keep everything, the minute still open included, for the app going to the background. */
  async flush(): Promise<void> {
    this.advanceShare();
    this.sweep();
    await this.persist();
  }

  /** Closed intervals not yet accepted, and the minute in progress. */
  get waiting(): number { return this.closed.length + (this.batch?.intervals.length ?? 0); }

  private persisting: Promise<void> = Promise.resolve();
  private persist(): Promise<void> {
    const saved: Saved = { closed: this.closed, minutes: [...this.minutes.values()], ...(this.batch ? { batch: this.batch } : {}) };
    const encoded = JSON.stringify(saved);
    this.persisting = this.persisting.then(() => this.storage.write(encoded)).catch(() => {});
    return this.persisting;
  }
}

function valid(interval: unknown): interval is Interval {
  const candidate = interval as Interval | null;
  return !!candidate && typeof candidate.start === 'number' && candidate.start % MINUTE_MS === 0 && !!candidate.series && !!candidate.shares;
}
