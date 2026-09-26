import assert from 'node:assert/strict';
import { listCollectionPages } from './collectionPages.ts';

const snapshot = { id: 'snapshot/1', host_id: 'host/a', store_index: 1, projection_version: 'client-projection.v0' };
const result = (items, more, next) => ({ snapshot, value: { items, page: { has_more: more, next_cursor: next } } });
const calls = [];
const complete = await listCollectionPages(async ({ cursor }) => {
  calls.push(cursor);
  return cursor ? result([{ id: 'attention/2' }], false, null) : result([{ id: 'attention/1' }], true, 'next');
}, 30);
assert.deepEqual(calls, [undefined, 'next']);
assert.equal(complete.truncated, false);
assert.equal(complete.pages.flatMap(page => page.value.items).length, 2);

let boundedCalls = 0;
const bounded = await listCollectionPages(async () => result([], true, `next-${++boundedCalls}`), 30, 3);
assert.equal(boundedCalls, 3);
assert.equal(bounded.truncated, true);

let firstPages = 0, secondPages = 0;
const retried = await listCollectionPages(async ({ cursor }) => {
  if (!cursor) { firstPages++; return result([], true, 'next'); }
  secondPages++;
  if (secondPages === 1) throw { response: { code: 'page-cursor-expired' } };
  return result([], false, null);
}, 30);
assert.equal(retried.truncated, false);
assert.equal(firstPages, 2);
assert.equal(secondPages, 2);
await assert.rejects(listCollectionPages(async () => result([], true, 'same'), 30), /did not advance/);

// (1) One forbidden collection must not blank the others or hide why it is missing.
const { settleCollections } = await import('./collectionPages.ts');
const ok = { pages: [], truncated: false };
const forbidden = Object.assign(new Error('device inventory requires an explicitly authenticated person'), { response: { code: 'forbidden' } });
const settled = settleCollections(['attention', 'devices'], await Promise.allSettled([Promise.resolve(ok), Promise.reject(forbidden)]), error => `${error.response.code}: ${error.message}`);
assert.deepEqual(settled.values, { attention: ok });
assert.deepEqual(settled.errors, { devices: 'forbidden: device inventory requires an explicitly authenticated person' });
assert.equal(settled.firstError, undefined);
const allFailed = settleCollections(['attention'], await Promise.allSettled([Promise.reject(forbidden)]), String);
assert.equal(allFailed.firstError, forbidden);

// A refresh reads its collections with bounded concurrency instead of bursting all of them at once.
const { withConcurrency } = await import('./collectionPages.ts');
let inFlight = 0, peak = 0;
const tasks = Array.from({ length: 10 }, (_, index) => async () => { inFlight++; peak = Math.max(peak, inFlight); await new Promise(resolve => setTimeout(resolve, 5)); inFlight--; if (index === 3) throw new Error('four'); return index; });
const outcomes = await withConcurrency(tasks, 4);
assert.equal(peak, 4);
assert.deepEqual(outcomes.map(result => result.status === 'fulfilled' ? result.value : result.reason.message), [0, 1, 2, 'four', 4, 5, 6, 7, 8, 9]);
