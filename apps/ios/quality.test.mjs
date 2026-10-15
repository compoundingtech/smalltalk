import assert from 'node:assert/strict';
import { test } from 'node:test';
import { readFileSync } from 'node:fs';
import { LocalQuality, QUALITY_HISTORY_KEY, QUALITY_MAX_BYTES, QUALITY_RUN_KEY, readQualityLaunchClock } from './quality.ts';
import { SLO_COUNTERS, SLO_HISTOGRAMS, isQualityHour } from '../../clients/typescript/st3-views/slo.ts';
import { qualityRows, qualityVisibility } from './qualityView.ts';

function fixture(mark = null, fails = false) {
  let now = 0, wall = Date.parse('2026-01-01T12:00:00Z');
  const values = new Map(mark ? [[QUALITY_RUN_KEY, mark]] : []), writes = [], frames = [], timers = new Set();
  const storage = { async getItem(key) { return values.get(key) ?? null; }, async setItem(key, value) { if (fails) throw new Error('disk unavailable'); values.set(key, value); writes.push(key); } };
  const clock = { now: () => now, wall: () => wall, after(fn, ms) { const timer = { fn, ms }; timers.add(timer); return timer; }, cancel(timer) { timers.delete(timer); }, frame(fn) { frames.push(fn); } };
  const quality = new LocalQuality(storage, clock, 'ios', 'build-test');
  return { quality, values, writes, timers, advance(ms) { now += ms; wall += ms; }, async paint() { while (frames.length) { const pending = frames.splice(0); now += 8; wall += 8; for (const fn of pending) fn(); } }, async rows() { await quality.flush(); return (values.get(QUALITY_HISTORY_KEY) ?? '').split('\n').filter(Boolean).map(JSON.parse); }, wall: () => wall };
}
const samples = (row, name) => row.histograms[name].reduce((a, b) => a + b, 0);

test('fixed recorder samples only painted, visible frames and not frames arriving after a commit', async () => {
  const f = fixture(); await f.quality.ready;
  f.quality.received(1, 0); f.quality.received(2, 0); f.quality.live(true);
  f.quality.commit(1, true, 2, 0);
  f.quality.received(1, 1); // not contained in that commit
  await f.paint();
  const row = (await f.rows()).at(-1);
  assert.equal(samples(row, 'freshness_ms'), 1);
  assert.equal(samples(row, 'first_frame_ms'), 1);
  assert.equal(samples(row, 'first_real_state_ms'), 1);
  assert.equal(samples(row, 'reconnect_ms'), 0);
  f.quality.key(); f.quality.commit(1, true, 2, 0); await f.paint();
  const next = (await f.rows()).at(-1);
  assert.equal(samples(next, 'freshness_ms'), 2);
  assert.equal(samples(next, 'key_to_redraw_ms'), 1);
  assert.equal(samples(next, 'first_frame_ms'), 1);
});
test('opens require visible entries / a visible attached terminal; cancellation and typed failures are distinct', async () => {
  const f = fixture(); await f.quality.ready;
  const cancelled = f.quality.beginConversation(); f.quality.endConversation(cancelled); f.quality.conversationVisible(cancelled);
  for (const code of ['transcript-not-bound', 'remote-unavailable', 'timeline-history-incomplete']) { const token = f.quality.beginConversation(); f.quality.conversationError(token, code); f.quality.conversationError(token, code); }
  const open = f.quality.beginConversation(); f.advance(100); f.quality.conversationVisible(open);
  const terminal = f.quality.beginTerminal(); f.quality.terminalVisible(terminal); await f.paint();
  let row = (await f.rows()).at(-1);
  assert.equal(samples(row, 'conversation_open_ms'), 1);
  assert.equal(samples(row, 'terminal_open_ms'), 0);
  assert.equal(row.counters.open_not_bound, 1); assert.equal(row.counters.open_remote_unavailable, 1); assert.equal(row.counters.open_cannot_load, 1);
  f.quality.terminalAttachment(terminal); await f.paint(); row = (await f.rows()).at(-1);
  assert.equal(samples(row, 'terminal_open_ms'), 1);
});
test('answer landing requires successful confirmation plus a complete fresh window in either order and visibility', async () => {
  const f = fixture(); await f.quality.ready;
  f.quality.commit(0); await f.paint();
  const first = f.quality.answerSent('attention/first');
  f.quality.attentionWindow([], false); await f.paint();
  assert.equal(samples((await f.rows()).at(-1), 'alert_landed_ms'), 0);
  f.quality.answerConfirmed('attention/first', first, true); await f.paint(); // hidden
  assert.equal(samples((await f.rows()).at(-1), 'alert_landed_ms'), 0);
  f.quality.commit(1); await f.paint();
  const second = f.quality.answerSent('attention/second'); f.quality.answerConfirmed('attention/second', second, true);
  f.quality.attentionWindow([], true); f.quality.commit(1); await f.paint();
  assert.equal(samples((await f.rows()).at(-1), 'alert_landed_ms'), 1);
  f.quality.attentionWindow([{ id: 'attention/second', state: 'resolved' }], false); f.quality.commit(1); await f.paint();
  const failed = f.quality.answerSent('attention/failed'); f.quality.attentionWindow([], false); f.quality.answerConfirmed('attention/failed', failed, false); f.quality.commit(1); await f.paint();
  const row = (await f.rows()).at(-1);
  assert.equal(samples(row, 'alert_landed_ms'), 2);
  assert.equal(JSON.stringify(row).includes('attention/'), false);
});
test('active time, non-live time and reconnect stop in background; markers distinguish fatal JS errors', async () => {
  const f = fixture('active'); await f.quality.ready;
  assert.equal(f.quality.wasUnclean, true);
  f.advance(100); f.quality.live(true); f.quality.commit(0); await f.paint();
  f.advance(100); f.quality.live(false); f.advance(40); f.quality.live(true); f.quality.commit(0); await f.paint();
  f.quality.foreground(false); await f.quality.flush();
  assert.equal(f.values.get(QUALITY_RUN_KEY), 'clean'); assert.equal(f.timers.size, 0);
  const before = (await f.rows()).at(-1).counters.active_ms;
  f.advance(5000); await f.quality.flush();
  let row = (await f.rows()).at(-1);
  assert.equal(row.counters.active_ms, before); assert.equal(row.counters.silent_exits, 1);
  assert.equal(samples(row, 'reconnect_ms'), 1);
  f.quality.foreground(true); f.quality.panic(); await f.quality.flush();
  assert.equal(f.values.get(QUALITY_RUN_KEY), 'panic'); row = (await f.rows()).at(-1); assert.equal(row.counters.panics, 1);
  const p = fixture('panic'); await p.quality.ready; assert.equal(p.quality.wasUnclean, true); assert.equal((await p.rows()).at(-1).counters.silent_exits, 0);
});
test('hourly writer is bounded and content-free, and Fleet aggregates only the last 24 completed clock hours', async () => {
  const f = fixture(); await f.quality.ready;
  assert.equal(f.timers.size, 1);
  for (let i = 0; i < 400; i++) { f.advance(3600_000); await f.quality.flush(); }
  const rows = (f.values.get(QUALITY_HISTORY_KEY)).split('\n').map(JSON.parse);
  assert.equal(rows.length, 336); assert.ok(f.values.get(QUALITY_HISTORY_KEY).length <= QUALITY_MAX_BYTES);
  for (const row of rows) { assert.equal(isQualityHour(row), true); assert.deepEqual(Object.keys(row.counters).sort(), [...SLO_COUNTERS].sort()); assert.deepEqual(Object.keys(row.histograms).sort(), [...SLO_HISTOGRAMS].sort()); }
  const report = f.quality.report();
  assert.equal(report.last.window_start, Math.floor(f.wall() / 3600_000) * 3600 - 3600);
  assert.equal(report.day.counters.active_ms, 24 * 3600_000);
  assert.equal(qualityRows(report.last, report.day).length, 14);
  f.advance(25 * 3600_000); assert.equal(f.quality.report().last, null); assert.equal(f.quality.report().day, null);
});
test('full numeric buffers report overflow; unavailable storage never rejects the app lifecycle', async () => {
  const f = fixture(null, true); await f.quality.ready;
  for (let i = 0; i < 130; i++) { f.quality.key(); f.quality.received(1, i); }
  f.quality.commit(1); await f.paint();
  assert.equal(f.quality.counters.line(0, 'ios', 'build-test').counters.measurement_overflow, 68);
  await f.quality.flush(); f.quality.foreground(false); await f.quality.flush(); f.quality.panic(); await f.quality.flush();
  assert.equal(f.writes.length, 0);
  // Measurement buffers remained fixed despite overflow; their drains are safe to serialize.
  assert.equal(f.quality.counters.line(0, 'ios', 'build-test').version, 1);
});

test('visibility excludes covered terminal/history screens, and includes only actual agent strips and badges', () => {
  const agents = [{ id: 'agent/example' }], sessions = [{ id: 'session/example', owner_id: 'agent/example', state: 'running' }];
  assert.equal(qualityVisibility('Terminal', true, undefined, agents, sessions), 0);
  assert.equal(qualityVisibility('FleetRoot', true, undefined, agents, sessions), 1);
  assert.equal(qualityVisibility('Agents', true, undefined, agents, sessions), 3);
  assert.equal(qualityVisibility('Conversation', true, 'session/example', agents, sessions), 3);
  assert.equal(qualityVisibility('Conversation', true, 'session/example', agents, [{ ...sessions[0], managed: false }]), 0);
  assert.equal(qualityVisibility('Conversation', true, 'session/absent', agents, sessions), 0);
  assert.equal(qualityVisibility('Agents', false, undefined, agents, sessions), 0);
});
test('hour boundary schedules exactly one next-hour wake and a missing native launch clock yields no invented sample', async () => {
  const f = fixture(); await f.quality.ready;
  const timer = [...f.timers][0]; assert.equal(timer.ms, 3600_000);
  f.advance(timer.ms); timer.fn(); await f.quality.flush();
  assert.equal(f.timers.size, 1); assert.equal([...f.timers][0].ms, 3600_000);
  assert.equal(f.quality.report().last.counters.active_ms, 3600_000);
  const clock = { now: () => 100, wall: () => Date.parse('2026-01-01T12:00:00Z'), after() {}, cancel() {}, frame(fn) { fn(); } };
  const noStart = new LocalQuality({ async getItem() { return null; }, async setItem() {} }, clock, 'ios', 'build-test', true, null);
  await noStart.ready; noStart.commit(3, true);
  const row = noStart.counters.line(0, 'ios', 'build-test');
  assert.equal(row.counters.measurement_overflow, 1); assert.equal(samples(row, 'first_frame_ms'), 0); assert.equal(samples(row, 'first_real_state_ms'), 0);
});

test('unavailable startup getters and synchronous native storage failures never reach the app', async () => {
  assert.equal(readQualityLaunchClock({ get rnStartupTiming() { throw new Error('native timing unavailable'); } }), null);
  assert.equal(readQualityLaunchClock({ rnStartupTiming: { startTime: 0 } }), 0);
  const storage = { getItem() { throw new Error('native storage unavailable'); }, setItem() { throw new Error('native storage unavailable'); } };
  const clock = { now: () => 0, wall: () => Date.parse('2026-01-01T12:00:00Z'), after() {}, cancel() {}, frame() {} };
  const recorder = new LocalQuality(storage, clock, 'ios', 'build-test');
  await recorder.ready; await recorder.flush(); recorder.foreground(false); await recorder.flush();
});

test('diagnostics wrapper preserves main quality instrumentation on the original root views (structure only)', () => {
  const source = readFileSync(new URL('./App.tsx', import.meta.url), 'utf8');
  assert.match(source, /export default function App\(\) \{\s*return <DiagnosticsBoundary><AppContent \/><\/DiagnosticsBoundary>;\s*\}/);
  const content = source.slice(source.indexOf('function AppContent()'), source.indexOf('export default function App()'));
  assert.match(content, /if \(!fontsLoaded\) return <View onLayout=\{qualityLayout\}/);
  assert.match(content, /return <SafeAreaProvider onLayout=\{qualityLayout\}>/);
  assert.match(source, /const qualityLayout = \(\) => quality\.commit\(\);/);
  assert.match(source, /useLayoutEffect\(\(\) => \{\s*if \(!proofSession\) quality\.commit\(shownMask,/);
  const boundary = readFileSync(new URL('./DiagnosticsBoundary.tsx', import.meta.url), 'utf8');
  assert.match(boundary, /componentDidCatch\(error: unknown\): void \{ captureJsError\(error, true\); \}/);
  assert.match(boundary, /if \(this\.state\.failed\) return <View/);
});
