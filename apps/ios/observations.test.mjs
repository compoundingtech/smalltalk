import assert from 'node:assert/strict';
import { KEEP_MS, MAX_INTERVALS_PER_REPORT, MAX_KEPT_INTERVALS, MAX_LATENCY_MS, MINUTE_MS, Observations, TARGETS, canonicalUpperMs, toObservationReport } from './observations.ts';

const T0 = Date.UTC(2026, 9, 10, 12, 0, 0); // a minute boundary
function make(options = {}) {
  const state = { now: T0 + 5_000, stored: options.stored ?? null, writes: 0, ids: 0, context: options.context ?? { carrier: 'fabric', path: 'direct' } };
  const storage = { read: async () => state.stored, write: async value => { state.stored = value; state.writes++; } };
  const clock = { now: () => state.now, id: () => `report-${++state.ids}` };
  const observations = new Observations(storage, clock, () => state.context);
  return { observations, state };
}
const settle = () => new Promise(resolve => setTimeout(resolve, 0));

// ---- Histogram: bins are named by the member's canonical inclusive upper bound, in whole ms.
// These pairs are what smallclaims::windows::histogram_upper_ms returns (pinned by a test there too).
for (const [ms, bound] of [[0, 0], [1, 1], [15, 15], [16, 16], [17, 17], [31, 31], [32, 33], [33, 33], [100, 103], [500, 511], [501, 511], [999, 1023], [1000, 1023], [1500, 1535], [3000, 3071], [8526, 8703], [10000, 10239], [59000, 59391], [3600000, 3670015]]) {
  assert.equal(canonicalUpperMs(ms), bound, `${ms}`);
}
assert.equal(canonicalUpperMs(MAX_LATENCY_MS + 5_000), canonicalUpperMs(MAX_LATENCY_MS), 'a latency past the member\'s limit is kept at it');
for (let ms = 0; ms < 70_000; ms += 7) {
  const bound = canonicalUpperMs(ms);
  assert.equal(canonicalUpperMs(bound), bound, `${ms} -> ${bound} is a bound of its own bin`);
  assert.ok(bound >= ms && bound - ms <= ms / 16, `${ms} -> ${bound}`);
}

/** The member's rules for a latency sample (crates/st3/src/api/client_observations.rs), as far as they bear on what the app sends. */
function memberAccepts(sample) {
  const p99 = TARGETS[sample.target];
  const lower = bound => bound < 16 ? bound : bound + 1 - 2 ** (31 - Math.clz32(bound) - 4);
  let previous = -1, sum = 0, definitelyOver = 0, possiblyOver = 0;
  for (const [bound, count] of sample.buckets) {
    if (count <= 0 || bound > canonicalUpperMs(MAX_LATENCY_MS) || canonicalUpperMs(bound) !== bound || previous >= bound) return 'histogram requires sorted unique canonical inclusive ms bin bounds and positive counts';
    sum += count;
    if (lower(bound) > p99) definitelyOver += count;
    if (bound > p99) possiblyOver += count;
    previous = bound;
  }
  if (sum !== sample.count || sample.over_target < definitelyOver || sample.over_target > possiblyOver || sample.buckets.at(-1)[0] !== canonicalUpperMs(sample.max_ms)
    || (sample.max_ms <= p99 && sample.over_target !== 0) || (sample.max_ms > p99 && sample.over_target === 0)) return 'histogram, max and over-target count disagree';
  return null;
}

// ---- What the app sends is what the member accepts, whatever the latencies are.
{
  const { observations, state } = make();
  for (const target of Object.keys(TARGETS)) {
    for (const ms of [1, 16, 17, 58, 89, 300, TARGETS[target], TARGETS[target] + 1, 1000, 2500, 8526, 9999, 70_000, MAX_LATENCY_MS * 2]) observations.observe(target, ms);
  }
  state.now = T0 + 2 * MINUTE_MS;
  const report = toObservationReport(observations.nextReport());
  const latency = report.samples.filter(sample => sample.kind === 'latency');
  assert.equal(latency.length, Object.keys(TARGETS).length);
  for (const sample of latency) assert.equal(memberAccepts(sample), null, sample.target);
}

// ---- A latency lands in its minute, on its route, with its misses counted against its target.
{
  const { observations, state } = make();
  const token = observations.begin('ios-message-ack');
  state.now += 120;
  observations.end(token);
  observations.observe('ios-message-ack', 900);
  observations.observe('ios-open-to-live', 1400, { carrier: 'tailscale' });
  observations.observe('ios-recover', -5);
  observations.observe('ios-recover', Number.NaN);
  assert.equal(observations.nextReport(), null, 'the minute in progress is not reported');
  state.now = T0 + MINUTE_MS + 1;
  const report = observations.nextReport();
  assert.equal(report.report_id, 'report-1');
  assert.equal(report.intervals.length, 1);
  const [interval] = report.intervals;
  assert.equal(interval.interval_start, '2026-10-10T12:00:00.000Z');
  assert.equal(interval.interval_end, '2026-10-10T12:01:00.000Z');
  const ack = interval.samples.find(sample => sample.target === 'ios-message-ack');
  assert.deepEqual({ ...ack, buckets: undefined }, { target: 'ios-message-ack', carrier: 'fabric', path: 'direct', count: 2, over_target: 1, max_ms: 900, buckets: undefined });
  assert.equal(ack.buckets.reduce((sum, [, count]) => sum + count, 0), 2);
  assert.ok(ack.buckets.every(([edge, count]) => Number.isInteger(edge) && count > 0));
  const open = interval.samples.find(sample => sample.target === 'ios-open-to-live');
  assert.deepEqual([open.carrier, open.path, open.count, open.over_target], ['tailscale', undefined, 1, 0]);
  assert.equal(interval.samples.length, 2, 'a negative or NaN latency is not a sample');
}

// ---- A closed interval is sent once, and a retry is the identical report, not a new one.
{
  const { observations, state } = make();
  observations.observe('ios-connect', 200);
  state.now = T0 + 2 * MINUTE_MS + 10;
  const first = observations.nextReport();
  // Samples after the batch was formed wait for the next one; the first is not rebuilt.
  observations.observe('ios-connect', 300);
  state.now += MINUTE_MS;
  const retry = observations.nextReport();
  assert.equal(JSON.stringify(retry), JSON.stringify(first));
  assert.equal(retry.report_id, 'report-1');
  observations.accepted('report-0');
  assert.equal(JSON.stringify(observations.nextReport()), JSON.stringify(first), 'another report ID accepts nothing');
  observations.accepted('report-1');
  const next = observations.nextReport();
  assert.equal(next.report_id, 'report-2');
  assert.equal(next.intervals.length, 1);
  assert.equal(next.intervals[0].interval_start, '2026-10-10T12:02:00.000Z');
  observations.accepted('report-2');
  assert.equal(observations.nextReport(), null);
  assert.equal(observations.waiting, 0);
}

// ---- A report is bounded; the rest wait for the next. Old intervals are dropped; the keep is bounded.
{
  const { observations, state } = make();
  for (let minute = 0; minute < MAX_INTERVALS_PER_REPORT + 5; minute++) { state.now = T0 + minute * MINUTE_MS + 100; observations.observe('ios-connect', 100); }
  state.now += 2 * MINUTE_MS;
  const first = observations.nextReport();
  assert.equal(first.intervals.length, MAX_INTERVALS_PER_REPORT);
  observations.accepted(first.report_id);
  assert.equal(observations.nextReport().intervals.length, 5);
}
{
  const { observations, state } = make();
  observations.observe('ios-connect', 100);
  state.now = T0 + KEEP_MS + 2 * MINUTE_MS;
  assert.equal(observations.nextReport(), null, 'seven days old: dropped, never sent late');
}
{
  const { observations, state } = make();
  for (let minute = 0; minute < MAX_KEPT_INTERVALS + 40; minute++) { state.now = T0 + minute * MINUTE_MS + 100; observations.observe('ios-connect', 100); }
  state.now += MINUTE_MS;
  observations.nextReport();
  assert.ok(observations.waiting <= MAX_KEPT_INTERVALS, 'the outbox is bounded while no member accepts');
}

// ---- The share of foreground time with a live feed: foreground is the denominator, live the part served.
{
  const { observations, state } = make();
  state.now = T0 + 50_000;
  observations.setForeground(true);
  state.now += 4_000;               // foreground, not live yet
  observations.setLive(true);
  state.now += 20_000;              // live across the minute boundary: 6 s in minute 0, 14 s in minute 1
  observations.setForeground(false);
  state.now += 30_000;              // background: counts for nothing
  observations.setForeground(true);
  state.now += 10_000;              // foreground and still live
  observations.setForeground(false);
  state.now = T0 + 2 * MINUTE_MS + 1;
  const report = observations.nextReport();
  const [m0, m1] = report.intervals;
  assert.deepEqual(m0.shares, [{ carrier: 'fabric', path: 'direct', foreground_ms: 10_000, live_ms: 6_000 }]);
  assert.equal(m1.shares[0].foreground_ms, 14_000 + 10_000);
  assert.equal(m1.shares[0].live_ms, 14_000 + 10_000);
  // The feed dropping while in the foreground is the miss.
}
{
  const { observations, state } = make();
  state.now = T0 + 1_000;
  observations.setForeground(true);
  observations.setLive(true);
  state.now += 10_000;
  observations.setLive(false);
  state.now += 5_000;
  observations.setForeground(false);
  state.now = T0 + MINUTE_MS + 1;
  const [interval] = observations.nextReport().intervals;
  assert.deepEqual([interval.shares[0].foreground_ms, interval.shares[0].live_ms], [15_000, 10_000]);
}

// ---- What a report holds: no content, address, node ID or credential, only these keys.
{
  const { observations, state } = make({ context: { carrier: 'lan' } });
  observations.observe('ios-terminal-open', 1200);
  observations.setForeground(true);
  state.now += 1000;
  state.now = T0 + MINUTE_MS + 1;
  const report = observations.nextReport();
  assert.deepEqual(Object.keys(report), ['report_id', 'intervals']);
  assert.deepEqual(Object.keys(report.intervals[0]), ['interval_start', 'interval_end', 'samples', 'shares']);
  assert.deepEqual(Object.keys(report.intervals[0].samples[0]).sort(), ['buckets', 'carrier', 'count', 'max_ms', 'over_target', 'target']);
  assert.deepEqual(Object.keys(report.intervals[0].shares[0]).sort(), ['carrier', 'foreground_ms', 'live_ms']);
  assert.ok(Object.keys(TARGETS).every(name => name.startsWith('ios-')));
}

// ---- Kept across a restart: the batch with its ID, the closed intervals, and the minute in progress.
{
  const first = make();
  first.observations.observe('ios-connect', 200);
  first.state.now = T0 + MINUTE_MS + 10;
  first.observations.observe('ios-connect', 250);          // in minute 1
  const report = first.observations.nextReport();           // forms a batch of minute 0
  first.state.now += 10;
  await first.observations.flush();
  await settle();
  const second = make({ stored: first.state.stored });
  second.state.now = T0 + MINUTE_MS + 20;                   // the same minute as the one left open
  second.observations.observe('ios-connect', 300);
  await second.observations.load();
  assert.equal(JSON.stringify(second.observations.nextReport()), JSON.stringify(report), 'the same batch, the same ID');
  second.observations.accepted(report.report_id);
  second.state.now = T0 + 3 * MINUTE_MS;
  const rest = second.observations.nextReport();
  assert.equal(rest.intervals.length, 1);
  const sample = rest.intervals[0].samples[0];
  assert.equal(sample.count, 2, 'the restored open minute and this run\'s sample are summed');
}
{
  const damaged = make({ stored: '{not json' });
  await damaged.observations.load();
  damaged.observations.observe('ios-connect', 100);
  damaged.state.now = T0 + MINUTE_MS + 1;
  assert.equal(damaged.observations.nextReport().intervals.length, 1, 'a damaged record is dropped and measuring goes on');
}

// ---- A minute that is closed twice is one interval: the member refuses a report with a minute twice.
{
  const { observations, state } = make();
  state.now = T0 + 10_000;
  observations.observe('ios-connect', 100);                 // minute 0
  state.now = T0 + 20_000;
  observations.setForeground(true);
  observations.setLive(true);
  state.now = T0 + MINUTE_MS + 10_000;
  observations.observe('ios-connect', 200);                 // minute 1 begins: minute 0 is closed, but the share is still in it
  state.now += 5_000;
  observations.setLive(false);                              // the rest of minute 0 is counted and it is closed again
  state.now = T0 + 3 * MINUTE_MS;
  const report = observations.nextReport();
  const starts = report.intervals.map(interval => interval.interval_start);
  assert.deepEqual(starts, [...new Set(starts)], 'no minute twice');
  const first = report.intervals[0];
  assert.equal(first.samples.length, 1);
  assert.equal(first.shares.length, 1);
  assert.equal(first.shares[0].foreground_ms, 40_000, 'both closes of minute 0 are summed');
}

// ---- A report the member refuses for its content is dropped; one it never saw is kept.
{
  const { observations, state } = make();
  observations.observe('ios-connect', 100);
  state.now = T0 + 2 * MINUTE_MS;
  const first = observations.nextReport();
  assert.equal(observations.waiting, 1);
  observations.refused('some-other-report');
  assert.equal(observations.nextReport().report_id, first.report_id, 'a refusal of another report changes nothing');
  observations.refused(first.report_id);
  assert.equal(observations.waiting, 0);
  assert.equal(observations.nextReport(), null, 'the refused batch is not sent again');
  observations.observe('ios-connect', 100);
  state.now = T0 + 4 * MINUTE_MS;
  assert.notEqual(observations.nextReport().report_id, first.report_id, 'later measurements are reported');
}

// ---- What an earlier build kept named its bins by index and the member never took it: it is not sent.
{
  const { observations, state } = make({ stored: JSON.stringify({ closed: [{ start: T0, series: { 'x|fabric|direct': { target: 'ios-connect', carrier: 'fabric', path: 'direct', count: 1, over_target: 0, max_ms: 100, buckets: { 106: 1 } } }, shares: {} }] }) });
  await observations.load();
  assert.equal(observations.waiting, 0);
  state.now = T0 + 2 * MINUTE_MS;
  assert.equal(observations.nextReport(), null);
}

// ---- Before a route is chosen nothing is reported as a carrier.
{
  const { observations, state } = make({ context: { carrier: 'none' } });
  observations.observe('ios-open-to-live', 900);
  state.now = T0 + 2 * MINUTE_MS;
  assert.equal(toObservationReport(observations.nextReport()).samples.length, 0);
}

// ---- A timer that is cancelled, or never ended, records nothing and does not pile up.
{
  const { observations, state } = make();
  const token = observations.begin('ios-recover');
  observations.cancel(token);
  state.now += 500;
  observations.end(token);
  for (let i = 0; i < 100; i++) observations.begin('ios-recover');
  state.now = T0 + MINUTE_MS + 1;
  assert.equal(observations.nextReport(), null);
}

// ---- A report is what the member takes: flat samples (at most 32), each naming its minute; an unknown carrier is left out.
{
  const { observations, state } = make({ context: { carrier: 'tailscale' } });
  const token = observations.begin('ios-message-ack');
  state.now += 120;
  observations.end(token);
  observations.observe('ios-open-to-live', 900, { carrier: 'https' });
  observations.setForeground(true);
  observations.setLive(true);
  state.now += 20_000;
  observations.setLive(false);
  state.now = T0 + 2 * MINUTE_MS + 100;
  const batch = observations.nextReport();
  const report = toObservationReport(batch);
  assert.equal(report.report_id, batch.report_id);
  assert.ok(report.samples.length >= 1 && report.samples.length <= 32);
  const latency = report.samples.find(sample => sample.kind === 'latency');
  assert.equal(latency.target, 'ios-message-ack');
  assert.equal(latency.carrier, 'tailscale');
  assert.equal(latency.interval_start, new Date(T0).toISOString());
  assert.equal(latency.count, 1);
  assert.ok(!report.samples.some(sample => sample.carrier === 'https'), 'a carrier the member does not know is not reported');
  for (const sample of report.samples.filter(sample => sample.kind === 'live-share')) {
    assert.equal(sample.target, 'ios-live-share');
    assert.ok(sample.foreground_ms >= 1 && sample.foreground_ms <= 60_000 && sample.live_ms <= sample.foreground_ms);
  }
}
{
  // Many targets in one minute never make a report over the member's 32 samples.
  const { observations, state } = make();
  for (let minute = 0; minute < 12; minute++) {
    state.now = T0 + minute * MINUTE_MS + 100;
    for (const target of Object.keys(TARGETS)) for (const carrier of ['fabric', 'tailscale', 'lan']) observations.observe(target, 100, { carrier });
  }
  state.now += 2 * MINUTE_MS;
  const report = toObservationReport(observations.nextReport());
  assert.ok(report.samples.length <= 32, `${report.samples.length}`);
}
