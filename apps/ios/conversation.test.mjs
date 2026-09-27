import assert from 'node:assert/strict';
import { conversationRows, loadRecentConversation } from './sessionView.ts';

// Pages arrive newest first; `next_cursor` continues toward older entries.
function timeline(entries, pageSize) {
  const calls = [];
  const list = async ({ limit, cursor }) => {
    calls.push({ limit, cursor });
    const newestFirst = [...entries].sort((a, b) => b.sequence - a.sequence);
    const offset = cursor ? Number(cursor) : 0;
    const items = newestFirst.slice(offset, offset + Math.min(limit, pageSize));
    const next = offset + items.length;
    return { value: { items, page: { has_more: next < newestFirst.length, next_cursor: next < newestFirst.length ? String(next) : null } } };
  };
  return { list, calls };
}
const status = sequence => ({ id: `entry/${sequence}`, revision: 1, sequence, type: 'status', role: 'system', body: { state: 'idle' } });
const content = (sequence, text) => ({ id: `entry/${sequence}`, revision: 1, sequence, type: 'content', role: 'assistant', body: { media_type: 'text/plain', text } });

// (4) A newest page full of status heartbeats must not hide the conversation behind it.
const heartbeats = Array.from({ length: 250 }, (_, index) => status(1000 + index));
const apple = timeline([content(10, 'Earlier reply'), content(20, 'Latest reply'), ...heartbeats], 100);
const loaded = await loadRecentConversation(apple.list, { pageSize: 100, maxPages: 5, want: 50 });
assert.deepEqual(loaded.entries.map(entry => entry.body.text), ['Earlier reply', 'Latest reply']);
assert.equal(loaded.hasOlder, false);

// Bounded: a session with no conversation stops after maxPages and says older history exists.
const empty = timeline(Array.from({ length: 1000 }, (_, index) => status(index)), 100);
const bounded = await loadRecentConversation(empty.list, { pageSize: 100, maxPages: 3, want: 50 });
assert.equal(empty.calls.length, 3);
assert.deepEqual(bounded.entries, []);
assert.equal(bounded.hasOlder, true);

// Stops as soon as enough conversation is visible.
const chatty = timeline(Array.from({ length: 400 }, (_, index) => content(index, `m${index}`)), 100);
const recent = await loadRecentConversation(chatty.list, { pageSize: 100, maxPages: 5, want: 50 });
assert.equal(chatty.calls.length, 1);
assert.equal(recent.entries.length, 50);
assert.equal(recent.entries.at(-1).body.text, 'm399');
assert.equal(recent.hasOlder, true);

// Incremental polls stop at what is already known instead of re-reading deep history.
const previous = await loadRecentConversation(apple.list, { pageSize: 100, maxPages: 5, want: 50 });
apple.calls.length = 0;
const again = await loadRecentConversation(apple.list, { pageSize: 100, maxPages: 5, want: 50, previous });
assert.equal(apple.calls.length, 1);
assert.deepEqual(again.entries.map(entry => entry.body.text), ['Earlier reply', 'Latest reply']);

// (6) The older-history marker renders above the oldest message, never below the newest.
const rows = conversationRows([content(1, 'old'), content(2, 'new')], true);
assert.deepEqual(rows.map(row => row.kind === 'older' ? 'older' : row.entry.body.text), ['older', 'old', 'new']);
assert.deepEqual(conversationRows([content(1, 'only')], false).map(row => row.kind), ['entry']);

// A streaming entry revised in place is picked up by the next incremental poll.
const streaming = [content(1, 'Hel'), content(2, 'Second')];
const stream = timeline(streaming, 100);
const before = await loadRecentConversation(stream.list, { pageSize: 100, maxPages: 5, want: 50 });
streaming[1] = { ...content(2, 'Second, finished'), revision: 2 };
const after = await loadRecentConversation(stream.list, { pageSize: 100, maxPages: 5, want: 50, previous: before });
assert.deepEqual(after.entries.map(entry => entry.body.text), ['Hel', 'Second, finished']);
