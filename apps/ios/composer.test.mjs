import assert from 'node:assert/strict';
import { draftSize, messageLimit, restoreDraft, sendReason } from './composer.ts';

// The limit is whatever st advertises; an older st advertises none and nothing is blocked here.
assert.equal(messageLimit({ limits: { max_message_bytes: 262144 } }), 262144);
assert.equal(messageLimit({ limits: {} }), undefined);
assert.equal(messageLimit(null), undefined);
assert.equal(draftSize('x'.repeat(500_000), undefined).over, false);

// Short messages say nothing; a long one says how long it is against the limit.
assert.equal(draftSize('hello', 262144).note, null);
assert.equal(draftSize('a'.repeat(8192), 262144).note, null);
assert.equal(draftSize('a'.repeat(9000), 262144).note, '8.8 KiB of 256 KiB');
assert.equal(draftSize('a'.repeat(9000), 262144).over, false);

// Bytes, not characters: 3-byte characters reach the limit at a third of its length.
const euros = '€'.repeat(Math.floor(262144 / 3));
assert.equal(draftSize(euros, 262144).over, false);
assert.equal(draftSize(`${euros}€`, 262144).over, true);
assert.equal(draftSize('a'.repeat(262144), 262144).over, false);
const over = draftSize('a'.repeat(262145), 262144);
assert.equal(over.over, true);
assert.match(over.note, /Too long to send: 256 KiB, and the limit is 256 KiB/);
assert.match(over.note, /text is kept/);
assert.equal(draftSize('🎉'.repeat(70000), 262144).over, true);

// A refused send puts the text back; what was typed meanwhile stays after it, and nothing is lost.
assert.equal(restoreDraft('', 'the long notes'), 'the long notes');
assert.equal(restoreDraft('  ', 'the long notes'), 'the long notes');
assert.equal(restoreDraft('and one more thing', 'the long notes'), 'the long notes\n\nand one more thing');
assert.equal(restoreDraft('the long notes', 'the long notes'), 'the long notes');
assert.equal(restoreDraft('typed', ''), 'typed');

// What the person reads.
assert.match(sendReason('st took too long to answer; it may or may not have done it', true), /Sending again will not send it twice/);
assert.match(sendReason('an inline message cannot exceed 4 KiB; post a document first', false), /only takes messages up to 4 KiB/);
assert.equal(sendReason('the full text is too large', false), 'the full text is too large. Your text is kept.');
