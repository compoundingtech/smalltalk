import assert from 'node:assert/strict';
import test from 'node:test';
import { watchSync, syncBannerText } from './syncWatch.ts';
import { alertsEmptyText } from './alertsEmpty.ts';

const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
const behind = { state: 'catching-up', peers: [{ host_id: 'host/harbor', peer_only_envelopes: 9, local_only_envelopes: 0 }] };

// A read happens at once and again each interval; each result is published, a failure as unknown.
await test('publishes each read, including failures as unknown', { timeout: 5_000 }, async t => {
  const seen = [];
  const results = [behind, null, 'boom', null];
  const { promise: published, resolve } = Promise.withResolvers();
  const stop = watchSync(async () => { const next = results.shift(); if (next === 'boom') throw new Error('down'); return next; }, (value, stale) => {
    seen.push(stale ? ['stale', value] : value);
    if (seen.length === 4) resolve();
  }, 10);
  t.after(stop);
  await published;
  stop();
  assert.deepEqual(seen, [behind, null, undefined, null]);
});

// A failed read after a behind notice keeps it, marked stale; a failed read after level, or with
// nothing read, is unknown; the next answer replaces either.
await test('keeps a behind notice stale until the next answer replaces it', { timeout: 5_000 }, async t => {
  const seen = [];
  const results = [behind, 'boom', 'boom', null, 'boom'];
  const { promise: published, resolve } = Promise.withResolvers();
  const stop = watchSync(async () => { const next = results.shift(); if (next === 'boom') throw new Error('down'); return next; }, (value, stale) => {
    seen.push(stale ? ['stale', value] : value);
    if (seen.length === 5) resolve();
  }, 10);
  t.after(stop);
  await published;
  stop();
  assert.deepEqual(seen, [behind, ['stale', behind], ['stale', behind], null, undefined]);
});
await test('publishes unknown when the first read fails', { timeout: 5_000 }, async t => {
  const seen = [];
  const { promise: published, resolve } = Promise.withResolvers();
  const stop = watchSync(async () => { throw new Error('down'); }, (value, stale) => {
    seen.push([value, stale]);
    resolve();
  }, 10);
  t.after(stop);
  await published;
  stop();
  assert.deepEqual(seen, [[undefined, false]]);
});

// A read that never answers counts as failed after the timeout, and the reads go on.
await test('continues reading after a hung read times out', { timeout: 5_000 }, async t => {
  const seen = [];
  let calls = 0;
  const { promise: published, resolve } = Promise.withResolvers();
  const stop = watchSync(() => { calls++; return calls === 1 ? new Promise(() => {}) : Promise.resolve(behind); }, (value, stale) => {
    seen.push([value, stale]);
    if (seen.length === 2) resolve();
  }, 10, 15);
  t.after(stop);
  await published;
  stop();
  assert.deepEqual(seen, [[undefined, false], [behind, false]]);
});

// The banner says when its notice was not refreshed, and says nothing for level or unknown.
assert.equal(syncBannerText(behind, false), 'Catching up: harbor has 9 envelopes this host lacks · estimating time to catch up. What shows here can be out of date until then.');
assert.equal(syncBannerText(behind, true), `${syncBannerText(behind, false)} Not refreshed: the last check failed.`);
assert.equal(syncBannerText(null, false), undefined);
assert.equal(syncBannerText(undefined, true), undefined);

// Stopping ends the reads and drops one that is still in flight.
{
  const seen = [];
  let calls = 0;
  const stop = watchSync(async () => { calls++; await wait(20); return null; }, value => seen.push(value), 10);
  stop();
  await wait(60);
  assert.equal(calls, 1);
  assert.deepEqual(seen, []);
}

// "All caught up" only for a loaded list on a notice that says level.
const online = { status: 'online', hasSynced: true };
assert.equal(alertsEmptyText({ ...online, sync: null }), 'No alerts. You’re all caught up.');
assert.equal(alertsEmptyText({ ...online, sync: { state: 'catching-up', peers: [{ host_id: 'host/dock', peer_only_envelopes: 0, local_only_envelopes: 0 }] } }), 'No alerts. You’re all caught up.');
assert.equal(alertsEmptyText({ ...online, sync: behind }), 'No alerts known. This view is out of date.');
assert.equal(alertsEmptyText({ ...online, sync: { state: 'diverged', peers: [{ host_id: 'host/quay', peer_only_envelopes: 0, local_only_envelopes: 0, diverged_since: '2026-09-29T10:00:00Z' }] } }), 'No alerts known. This view is out of date.');
assert.equal(alertsEmptyText({ ...online, sync: undefined }), 'No alerts here yet. Checking whether this phone is up to date…');
assert.equal(alertsEmptyText({ status: 'online', hasSynced: false, sync: null }), 'Checking for alerts…');
assert.equal(alertsEmptyText({ status: 'offline', hasSynced: true, sync: null }), 'No alerts in the offline snapshot.');
assert.equal(alertsEmptyText({ status: 'offline', hasSynced: false, sync: null }), 'Connect to check for alerts.');
assert.equal(alertsEmptyText({ ...online, sync: null, loadError: 'timed out' }), 'Alerts could not be loaded: timed out');
