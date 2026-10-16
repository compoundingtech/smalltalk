import assert from 'node:assert/strict';
import { ClientError } from '../../clients/typescript/st3-client/index.ts';
import { wholeText, wholeTextFailure } from './messageBody.ts';

const refused = (code, message = 'm') => new ClientError({ api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', request_id: 'request/x', code, message, retryable: false, details: {} }, 404);

assert.deepEqual(wholeText({ text: 'all of it' }), { text: 'all of it' });
assert.deepEqual(wholeText({ text: 'all of it', complete: true }), { text: 'all of it' });
const partial = wholeText({ text: 'the start…', complete: false });
assert.equal(partial.text, 'the start…');
assert.match(partial.note, /Only the start of this message can be shown/);

// st's own sentence for an expired body is kept; the others are worded here, with no codes and no "image".
assert.equal(wholeTextFailure(refused('blob-expired', 'the full text of this message expired and was removed from Juniper after its retention; only the preview remains')), 'the full text of this message expired and was removed from Juniper after its retention; only the preview remains');
assert.match(wholeTextFailure(refused('blob-not-found')), /no longer on the machine that took it/);
assert.match(wholeTextFailure(refused('remote-unavailable', 'owner host/Juniper is temporarily unavailable')), /^Juniper cannot be reached right now, so only the start/);
for (const code of ['blob-expired', 'blob-not-found', 'remote-unavailable', 'not-found', 'forbidden']) assert.doesNotMatch(wholeTextFailure(refused(code)), /image/i, code);
assert.equal(wholeTextFailure(new TypeError('Network request failed')), 'st cannot be reached right now');
