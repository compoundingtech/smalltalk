import assert from 'node:assert/strict';
import { applyConversation, applyOlderPage, conversationRows, olderFailed, olderLoading, olderNote, readOlder } from '@smalltalk/st3-views/sessionView';

const status = sequence => ({ id: `entry/${sequence}`, revision: 1, sequence, type: 'status', role: 'system', body: { state: 'idle' } });
const content = (sequence, text) => ({ id: `entry/${sequence}`, revision: 1, sequence, type: 'content', role: 'assistant', body: { media_type: 'text/plain', text } });
const message = (sequence, title) => ({ id: `entry/${sequence}-message`, revision: 1, sequence, type: 'message', role: 'user', body: { message_id: `message/${sequence}`, from: 'agent/example/lead', to: 'agent/example/worker', title } });
const texts = conversation => conversation.entries.map(entry => entry.body.text ?? entry.body.title);

// A newest page full of status heartbeats does not hide the conversation in it, and Small Talk
// joined by st is part of the conversation.
const page = [status(30), content(20, 'Latest reply'), message(15, 'Review the plan'), status(12), content(10, 'Earlier reply')];
const loaded = applyConversation(undefined, { replace: true, items: page, hasMore: false });
assert.deepEqual(texts(loaded), ['Earlier reply', 'Review the plan', 'Latest reply']);
assert.equal(loaded.hasOlder, false);
assert.equal(loaded.newestSequence, 30);

// Changes merge into what is held; a streaming entry revised in place replaces its revision.
const changed = applyConversation(loaded, { replace: false, items: [{ ...content(20, 'Latest reply, finished'), revision: 2 }, content(31, 'New turn'), status(32)], hasMore: false });
assert.deepEqual(texts(changed), ['Earlier reply', 'Review the plan', 'Latest reply, finished', 'New turn']);
assert.equal(changed.newestSequence, 32);

// A page that does not reach the start marks older history, and a later change keeps that mark.
const partial = applyConversation(undefined, { replace: true, items: [content(40, 'm40')], hasMore: true });
assert.equal(partial.hasOlder, true);
assert.equal(applyConversation(partial, { replace: false, items: [content(41, 'm41')], hasMore: false }).hasOlder, true);

// A new page replaces everything held, including the older-history mark.
const replaced = applyConversation(changed, { replace: true, items: [content(50, 'Fresh')], hasMore: false });
assert.deepEqual(texts(replaced), ['Fresh']);
assert.equal(replaced.hasOlder, false);

// Bounded: only the newest entries are kept, and the rest are marked as older history.
const chatty = applyConversation(undefined, { replace: true, items: Array.from({ length: 150 }, (_, index) => content(index, `m${index}`)), hasMore: false }, 50);
assert.equal(chatty.entries.length, 50);
assert.equal(texts(chatty).at(-1), 'm149');
assert.equal(chatty.hasOlder, true);

// The older-history marker renders above the oldest message, never below the newest.
const rows = conversationRows([content(1, 'old'), content(2, 'new')], true);
assert.deepEqual(rows.map(row => row.kind === 'older' ? 'older' : row.entry.body.text), ['older', 'old', 'new']);
assert.deepEqual(conversationRows([content(1, 'only')], false).map(row => row.kind), ['entry']);

// Reading back: an earlier page goes above what is held and is never dropped; held entries win.
const window = (items, hasMore) => ({ replace: true, items, hasMore, sessionId: 'session/a' });
let back = applyConversation(undefined, window([content(3, 'c'), content(4, 'd')], true));
assert.equal(olderNote(back), 'Scroll up for earlier entries');
back = olderLoading(back);
assert.equal(olderNote(back), 'Loading earlier entries…');
back = applyOlderPage(back, 'session/a', { items: [content(2, 'b'), { ...content(3, 'stale'), revision: 0 }], hasMore: true, cursor: 'cursor-1' }, 1000);
assert.deepEqual(texts(back), ['b', 'c', 'd']);
assert.equal(back.hasOlder, true);
assert.deepEqual(back.older.cursor, { value: 'cursor-1', at: 1000 });
back = applyOlderPage(back, 'session/a', { items: [content(1, 'a')], hasMore: false });
assert.equal(back.hasOlder, false);
assert.match(olderNote(back), /^Start of this session/);
// A reconnect's newest page that meets what is held keeps the earlier pages; one that skipped
// past it would leave a hole, so they go.
back = applyConversation(back, window([content(4, 'd'), content(5, 'e')], true));
assert.deepEqual(texts(back), ['a', 'b', 'c', 'd', 'e']);
assert.equal(back.hasOlder, false);
const skipped = applyConversation(back, window([content(9, 'x')], true));
assert.deepEqual(texts(skipped), ['x']);
assert.equal(skipped.hasOlder, true);

// Projection notices have session-stable IDs but do not connect disconnected history windows.
const notice = sequence => ({ id: 'timeline-entry/session/a/timeline-query-limited', revision: 1, sequence, type: 'error', role: 'system', body: { code: 'timeline-query-limited', message: 'Older operations are outside this view', retryable: false } });
let withNotice = applyConversation(undefined, window([content(3, 'c'), content(4, 'd'), notice(6)], true));
withNotice = applyOlderPage(withNotice, 'session/a', { items: [content(1, 'a'), content(2, 'b')], hasMore: false });
withNotice = applyConversation(withNotice, window([content(4, 'd'), content(5, 'e'), notice(6)], true));
assert.deepEqual(withNotice.entries.map(entry => entry.id), ['entry/1', 'entry/2', 'entry/3', 'entry/4', 'entry/5', notice(6).id]);
assert.equal(withNotice.older.paged, true);
withNotice = applyConversation(withNotice, window([content(9, 'x'), notice(10)], true));
assert.deepEqual(withNotice.entries.map(entry => entry.id), ['entry/9', notice(10).id]);
assert.equal(withNotice.older.paged, false);
assert.equal(withNotice.hasOlder, true);

// A page for another session is dropped; a failure says why.
assert.deepEqual(texts(applyOlderPage(skipped, 'session/b', { items: [content(8, 'w')], hasMore: true })), ['x']);
assert.match(olderNote(olderFailed(skipped, 'st did not answer')), /^Could not load earlier entries: st did not answer/);
// Paged entries are kept past the live bound.
const many = applyOlderPage(applyConversation(undefined, window([content(100, 'new')], true), 2), 'session/a', { items: [content(97, 'x'), content(98, 'y'), content(99, 'z')], hasMore: true });
assert.equal(applyConversation(many, { replace: false, items: [content(101, 'newer')], hasMore: true }, 2).entries.length, 5);

// A live cursor continues; an expired one (or none) reads again from the newest until a page
// reaches past the oldest entry held.
const expired = Object.assign(new Error('gone'), { response: { code: 'page-cursor-expired' } });
const pages = { undefined: { items: [content(5, 'e'), content(6, 'f')], hasMore: true, cursor: 'p2' }, p2: { items: [content(3, 'c'), content(4, 'd')], hasMore: true, cursor: 'p3' } };
const asked = [];
const read = async cursor => { asked.push(cursor); if (cursor === 'old') throw expired; return pages[cursor]; };
const oldest = content(5, 'e');
let got = await readOlder(read, { paged: true, start: false, loading: false, cursor: { value: 'old', at: 0 } }, oldest, 1);
assert.deepEqual(asked, ['old', undefined, 'p2']);
assert.deepEqual(got.items.map(entry => entry.body.text), ['c', 'd']);
asked.length = 0;
got = await readOlder(read, { paged: true, start: false, loading: false, cursor: { value: 'p2', at: 0 } }, oldest, 1);
assert.deepEqual(asked, ['p2']);
asked.length = 0;
await readOlder(read, { paged: true, start: false, loading: false, cursor: { value: 'p2', at: 0 } }, oldest, 300_000);
assert.deepEqual(asked, [undefined, 'p2'], 'a cursor st has let go is not tried');
