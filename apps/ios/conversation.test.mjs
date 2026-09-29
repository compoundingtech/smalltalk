import assert from 'node:assert/strict';
import { applyConversation, conversationRows } from './sessionView.ts';

const status = sequence => ({ id: `entry/${sequence}`, revision: 1, sequence, type: 'status', role: 'system', body: { state: 'idle' } });
const content = (sequence, text) => ({ id: `entry/${sequence}`, revision: 1, sequence, type: 'content', role: 'assistant', body: { media_type: 'text/plain', text } });
const message = (sequence, title) => ({ id: `entry/${sequence}-message`, revision: 1, sequence, type: 'message', role: 'user', body: { message_id: `message/${sequence}`, from: 'agent/fleet/lead', to: 'agent/fleet/worker', title } });
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
