import assert from 'node:assert/strict';
import { test } from 'node:test';
import { BATCH_BYTES, DIAGNOSTICS_KEY, DiagnosticsQueue, MAX_AGE_MS, QUEUE_BYTES, QUEUE_REPORTS, boundQueue, decodeQueue, diagnosticBatch, retryDelay } from './diagnosticsQueue.ts';

const now = 1_800_000_000_000;
const report = (i, captured = now) => ({
  event_id: `12345678-1234-8234-8234-${i.toString(16).padStart(12, '0')}`, launch_id: '12345678-1234-4234-8234-123456789abc', sequence: i,
  occurred_at_unix_ms: captured, captured_at_unix_ms: captured, occurrence_time_basis: 'exact', launch_id_basis: 'process',
  app_version: '1.0', native_build: '42', runtime_version: 'ios-42', update_id: 'embedded', platform: 'ios', os_version: '27', severity: 'info', capture_source: 'js',
  payload: { kind: 'launch', breadcrumb: 'js-start', inferred: false },
});
const state = events => ({ version: 1, events, failures: 0, retry_at: 0 });
const memoryStorage = () => {
  let text = null;
  return { read: async () => text, write: async next => { text = next; }, get text() { return text; } };
};

test('queue namespace, retention, count, oldest eviction and duplicate IDs', () => {
  assert.equal(DIAGNOSTICS_KEY, 'st3.client-diagnostics.v1');
  const bounded = boundQueue(state([report(999, now - MAX_AGE_MS), ...Array.from({ length: 140 }, (_, i) => report(i, now - 140 + i)), report(139), report(1000, now + 10 * 60 * 1000)]), now);
  assert.equal(bounded.events.length, QUEUE_REPORTS);
  assert.equal(bounded.events[0].sequence, 12);
  assert.equal(bounded.events.at(-1).sequence, 139);
  assert.equal(new Set(bounded.events.map(event => event.event_id)).size, QUEUE_REPORTS);
});

test('queue and batch byte budgets include JSON envelopes', () => {
  // Exercise the byte-budget defense directly; enqueue sanitizes arbitrary keys away.
  const large = Array.from({ length: 128 }, (_, i) => ({ ...report(i), extra: 'x'.repeat(10_000) }));
  const bounded = boundQueue(state(large), now);
  assert.ok(JSON.stringify(bounded).length <= QUEUE_BYTES);
  assert.ok(bounded.events.length < 128);
  assert.equal(bounded.events.at(-1).sequence, 127);
  const batch = diagnosticBatch(large);
  assert.ok(JSON.stringify(batch).length <= BATCH_BYTES);
  assert.ok(batch.events.length < 32);
  assert.equal(diagnosticBatch(Array.from({ length: 100 }, (_, i) => report(i))).events.length, 32);
});

test('persisted input is versioned, redacted, bounded, and expires after seven days', () => {
  const dirty = { ...report(1), credential: 'must-not-persist', payload: { ...report(1).payload, message: 'private' } };
  const decoded = decodeQueue(JSON.stringify({ ...state([dirty]), failures: 999, retry_at: now + 999_999_999 }), now);
  assert.deepEqual(decoded.events, [report(1)]);
  assert.equal(decoded.failures, 16);
  assert.equal(decoded.retry_at, now + 300_000);
  assert.equal(decodeQueue(JSON.stringify(state([report(1, now - MAX_AGE_MS)])), now).events.length, 0);
  assert.equal(decodeQueue('{broken', now).events.length, 0);
  assert.equal(decodeQueue(JSON.stringify({ ...state([report(1)]), version: 0 }), now).events.length, 0);
  assert.equal(decodeQueue('x'.repeat(QUEUE_BYTES + 1), now).events.length, 0);
});

test('concurrent enqueue and ack preserve reports captured during network I/O', async () => {
  const storage = memoryStorage(); const queue = new DiagnosticsQueue(storage, () => now, () => 1);
  await Promise.all([queue.enqueue([report(1)]), queue.enqueue([report(2)])]);
  let release, started;
  const inFlight = new Promise(resolve => { release = resolve; });
  const sent = new Promise(resolve => { started = resolve; });
  const flush = queue.flush(async batch => { started(batch); return inFlight; });
  const batch = await sent;
  assert.deepEqual(batch.events.map(event => event.sequence), [1, 2]);
  await queue.enqueue([report(3)]);
  release({ acknowledged_event_ids: [report(1).event_id, report(3).event_id, 'arbitrary-id'] });
  assert.equal(await flush, now);
  assert.deepEqual(JSON.parse(storage.text).events.map(event => event.sequence), [2, 3]);
  // Relaunch uses the durable remainder rather than memory-only ack removal.
  const reopened = new DiagnosticsQueue(storage, () => now);
  await reopened.flush(async remaining => ({ acknowledged_event_ids: remaining.events.map(event => event.event_id) }));
  assert.equal(JSON.parse(storage.text).events.length, 0);
});

test('native transfer returns IDs only after successful durable persistence', async () => {
  let fail = true; let saved = null;
  const queue = new DiagnosticsQueue({ read: async () => saved, write: async text => { if (fail) throw new Error('disk unavailable'); saved = text; } }, () => now);
  await assert.rejects(queue.enqueue([report(1)]));
  assert.equal(saved, null);
  fail = false;
  assert.deepEqual(await queue.enqueue([report(1)]), [report(1).event_id]);
  assert.deepEqual(await queue.enqueue([report(1)]), [report(1).event_id]);
  assert.equal(JSON.parse(saved).events.length, 1);
  assert.deepEqual(await queue.enqueue([report(2, now - MAX_AGE_MS)]), []);
});

test('failed ack writes retain events and retry duplicate ingestion after backoff', async () => {
  let saved = null; let failRemoval = true; let clock = now;
  const queue = new DiagnosticsQueue({ read: async () => saved, write: async text => {
    if (failRemoval && saved && JSON.parse(text).events.length === 0) throw new Error('disk unavailable'); saved = text;
  } }, () => clock, () => 1);
  await queue.enqueue([report(1)]);
  const send = async batch => ({ acknowledged_event_ids: batch.events.map(event => event.event_id) });
  assert.equal(await queue.flush(send), now + 1000);
  assert.equal(JSON.parse(saved).events.length, 1);
  failRemoval = false; clock += 1000;
  assert.equal(await queue.flush(send), undefined);
  assert.equal(JSON.parse(saved).events.length, 0);
});

test('transport/no-progress failures back off durably, eligibility gates every send', async () => {
  const storage = memoryStorage(); let clock = now; let sends = 0;
  const queue = new DiagnosticsQueue(storage, () => clock, () => 1);
  await queue.enqueue([report(1)]);
  const send = async () => { sends++; throw new Error('offline'); };
  assert.equal(await queue.flush(send, () => false), undefined); assert.equal(sends, 0);
  assert.equal(await queue.flush(send), now + 1000); assert.equal(sends, 1);
  assert.equal(await queue.flush(send), now + 1000); assert.equal(sends, 1);
  clock += 1000;
  const reopened = new DiagnosticsQueue(storage, () => clock, () => 1);
  assert.equal(await reopened.flush(async () => ({ acknowledged_event_ids: ['not-sent'] })), now + 3000);
  assert.equal(JSON.parse(storage.text).events.length, 1);
  assert.equal(retryDelay(1, 1), 1000);
  assert.equal(retryDelay(2, 1), 2000);
  assert.equal(retryDelay(16, 1), 300_000);
  assert.equal(retryDelay(16, 0), 225_000);
});

test('in-flight upload remains single, a replacement context resumes pending work', async () => {
  const storage = memoryStorage(); const queue = new DiagnosticsQueue(storage, () => now);
  await queue.enqueue([report(1), report(2)]);
  let release, started; let active = true;
  const pending = new Promise(resolve => { release = resolve; });
  const begin = new Promise(resolve => { started = resolve; });
  const first = queue.flush(async () => { started(); return pending; }, () => active);
  await begin; active = false;
  let replacementSends = 0;
  const second = queue.flush(async batch => { replacementSends++; return { acknowledged_event_ids: batch.events.map(event => event.event_id) }; });
  release({ acknowledged_event_ids: [report(1).event_id] });
  await Promise.all([first, second]);
  assert.equal(replacementSends, 1);
  assert.equal(JSON.parse(storage.text).events.length, 0);
});
