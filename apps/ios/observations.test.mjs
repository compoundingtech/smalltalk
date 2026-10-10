import assert from 'node:assert/strict';
import { BUCKETS_PER_DOUBLING, KEEP_MS, MAX_INTERVALS_PER_REPORT, MAX_KEPT_INTERVALS, MINUTE_MS, Observations, TARGETS, bucketEdge, bucketOf } from './observations.ts';

const T0 = Date.UTC(2026, 9, 10, 12, 0, 0); // a minute boundary
function make(options = {}) {
  const state = { now: T0 + 5_000, stored: options.stored ?? null, writes: 0, ids: 0, context: options.context ?? { carrier: 'fabric', path: 'direct' } };
  const storage = { read: async () => state.stored, write: async value => { state.stored = value; state.writes++; } };
  const clock = { now: () => state.now, id: () => `report-${++state.ids}` };
  const observations = new Observations(storage, clock, () => state.context);
  return { observations, state };
}
const settle = () => new Promise(resolve => setTimeout(resolve, 0));

// ---- Histogram: 16 buckets for each doubling, a bucket reported by its upper edge in whole ms.
assert.equal(BUCKETS_PER_DOUBLING, 16);
assert.equal(bucketOf(1), 0);
assert.equal(bucketOf(2), 16);
assert.equal(bucketOf(1000), Math.floor(16 * Math.log2(1000)));
for (const ms of [1, 3, 58, 89, 300, 1000, 2500, 9999]) {
  const edge = bucketEdge(bucketOf(ms));
  assert.ok(edge >= ms && edge <= Math.ceil(ms * 2 ** (1 / 16)) + 1, `${ms} -> ${edge}`);
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
