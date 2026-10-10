import assert from 'node:assert/strict';
import { ClientError, appliedAnswer, isTransient, isTransientCode, notApplied, outcomeUnknown, plainError, plainMessage, retryTransient } from '../../clients/typescript/st3-client/index.ts';

// st's errors read as sentences, as in the Rust client: no codes reach a person.
assert.equal(plainMessage('stale-fence', 'the client snapshot changed before the action was submitted'), 'st changed while this was on its way, so it was not applied');
assert.equal(plainMessage('remote-unavailable', 'owner host/Juniper is temporarily unavailable; cached data remains usable'), 'Juniper cannot be reached right now');
assert.equal(plainMessage('not-found', 'agent `agent/x` does not exist'), 'it is gone: agent `agent/x` does not exist');
assert.equal(plainMessage(undefined, 'invalid subscription'), 'invalid subscription');
const refused = code => new ClientError({ api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', request_id: 'request/x', code, message: 'm', retryable: false, details: {} }, 409);
assert.equal(plainError(refused('forbidden')), 'not allowed: m');
assert.equal(plainError(new TypeError('Network request failed')), 'st cannot be reached right now');

assert.ok(isTransient(refused('stale-fence')) && !isTransient(refused('not-found')));
assert.ok(isTransientCode(undefined) && isTransientCode('page-cursor-expired') && !isTransientCode('forbidden'));
// A terminal out of reach comes back; one whose process exited does not.
assert.ok(isTransientCode('terminal-unavailable') && !isTransientCode('terminal-ended'));
assert.equal(plainMessage('terminal-ended', 'the terminal process exited'), 'the terminal ended: its process exited');
assert.ok(notApplied(refused('stale-fence')) && !notApplied(new TypeError('Network request failed')), 'a lost answer may have applied');

// A race is tried again with a fresh request; a refusal is not.
let tries = 0;
assert.equal(await retryTransient(5, async () => { if (tries++ < 2) throw refused('stale-fence'); return 'sent'; }, notApplied), 'sent');
assert.equal(tries, 3);
tries = 0;
await assert.rejects(retryTransient(5, async () => { tries++; throw refused('forbidden'); }, notApplied));
assert.equal(tries, 1);

assert.equal(isTransient(refused('idempotency-key-expired')), false);
assert.equal(notApplied(refused('idempotency-key-expired')), false);
let expiredTries = 0;
await assert.rejects(retryTransient(8, async () => { expiredTries++; throw refused('idempotency-key-expired'); }));
assert.equal(expiredTries, 1, 'an expired committed request must never be retried with a fresh key');

// A send st may have taken, and one it never queued (the daemon says which in `details.applied`;
// a code this client does not know is still an error with that answer).
const answered = (code, applied, status = 503) => new ClientError({ api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', request_id: 'request/x', code, message: 'the database has not confirmed the send', retryable: false, details: { applied, action_id: 'action/a', idempotency_key: 'key/a' } }, status);
const unconfirmed = answered('message-send-unconfirmed', 'unknown');
assert.equal(appliedAnswer(unconfirmed), 'unknown');
assert.ok(outcomeUnknown(unconfirmed) && !notApplied(unconfirmed), 'it may still complete: repeat the identical request, never a new one');
assert.equal(plainError(unconfirmed), 'st may have taken it and has not confirmed it yet; sending it again is safe');
const queueFull = answered('rate-limited', 'none', 429);
assert.ok(notApplied(queueFull) && !outcomeUnknown(queueFull), 'nothing was queued: a fresh request is safe');
assert.ok(notApplied(answered('some-future-code', 'none')), 'the answer decides, not the code');
assert.equal(appliedAnswer(refused('forbidden')), undefined);
assert.ok(!outcomeUnknown(refused('forbidden')), 'a plain refusal is not an unknown outcome');
assert.ok(outcomeUnknown(new TypeError('Network request failed')), 'a lost answer is an unknown outcome');
