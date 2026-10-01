import assert from 'node:assert/strict';
import { ClientError, isTransient, isTransientCode, notApplied, plainError, plainMessage, retryTransient } from '../../clients/typescript/st3-client/index.ts';

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
assert.ok(notApplied(refused('stale-fence')) && !notApplied(new TypeError('Network request failed')), 'a lost answer may have applied');

// A race is tried again with a fresh request; a refusal is not.
let tries = 0;
assert.equal(await retryTransient(5, async () => { if (tries++ < 2) throw refused('stale-fence'); return 'sent'; }, notApplied), 'sent');
assert.equal(tries, 3);
tries = 0;
await assert.rejects(retryTransient(5, async () => { tries++; throw refused('forbidden'); }, notApplied));
assert.equal(tries, 1);
