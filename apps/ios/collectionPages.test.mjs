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
