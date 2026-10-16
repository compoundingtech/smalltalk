import assert from 'node:assert/strict';
import { watchSync } from './syncWatch.ts';
import { alertsEmptyText } from './alertsEmpty.ts';

const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
const behind = { state: 'catching-up', peers: [{ host_id: 'host/harbor', peer_only_envelopes: 9, local_only_envelopes: 0 }] };

// A read happens at once and again each interval; each result is published, a failure as unknown.
{
  const seen = [];
  const results = [behind, null, 'boom', null];
  const stop = watchSync(async () => { const next = results.shift(); if (next === 'boom') throw new Error('down'); return next; }, value => seen.push(value), 10);
  await wait(80);
  stop();
  assert.deepEqual(seen.slice(0, 4), [behind, null, undefined, null]);
}

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
