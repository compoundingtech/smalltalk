import assert from 'node:assert/strict';
import { ClientError } from '../../clients/typescript/st3-client/index.ts';
import { submitAnswer } from './answer.ts';

const refused = (code, details = {}) => new ClientError({ api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', request_id: 'request/x', code, message: 'm', retryable: false, details }, 409);

// A scripted st: each send answers the next step; `waiting` is what stillWaiting reports.
function world(steps, waiting) {
  const log = { sent: [], built: 0, waited: [], asked: 0 };
  const ports = {
    async build() { log.built++; return { id: `action/${log.built}` }; },
    async send(request) {
      log.sent.push(request.id);
      const step = steps[Math.min(log.sent.length - 1, steps.length - 1)];
      if (step instanceof Error) throw step;
      return step;
    },
    async stillWaiting() { log.asked++; const value = typeof waiting === 'function' ? waiting(log) : waiting; if (value instanceof Error) throw value; return value; },
    async wait(ms) { log.waited.push(ms); },
  };
  return { ports, log };
}

// An answer that lands is sent once.
{
  const { ports, log } = world(['ok'], true);
  assert.deepEqual(await submitAnswer(ports), { ok: true, how: 'sent' });
  assert.deepEqual(log.sent, ['action/1']);
  assert.equal(log.asked, 0, 'a success asks nothing');
}

// Cos, 2026-10-10: the answer landed but the reply was lost. The identical request is sent again
// (the same key), and st's first answer comes back: a success, no second record.
{
  const { ports, log } = world([new TypeError('Network request failed'), 'ok'], true);
  assert.deepEqual(await submitAnswer(ports), { ok: true, how: 'sent' });
  assert.deepEqual(log.sent, ['action/1', 'action/1'], 'the same request, not a rebuilt one');
  assert.equal(log.built, 1);
}

// st said it may still complete: the same.
{
  const { ports, log } = world([refused('unavailable', { applied: 'unknown' }), 'ok'], true);
  assert.deepEqual(await submitAnswer(ports), { ok: true, how: 'sent' });
  assert.deepEqual(log.sent, ['action/1', 'action/1']);
}

// The reply is lost twice, but the step is no longer waiting: it landed. Answered, not failed.
{
  const { ports } = world([new TypeError('Network request failed')], false);
  assert.deepEqual(await submitAnswer(ports), { ok: true, how: 'already' });
}

// A repeat submit after the answer landed: st refuses the new key as stale. The step no longer
// waits, so it is a success.
{
  const { ports, log } = world([refused('stale-fence')], false);
  assert.deepEqual(await submitAnswer(ports), { ok: true, how: 'already' });
  assert.deepEqual(log.sent, ['action/1'], 'no pointless retries once it is answered');
}

// A stale fence while it still waits is a race: a fresh request each time, then the refusal.
{
  const { ports, log } = world([refused('stale-fence'), 'ok'], true);
  assert.deepEqual(await submitAnswer(ports), { ok: true, how: 'sent' });
  assert.deepEqual(log.sent, ['action/1', 'action/2']);
  const stuck = world([refused('stale-fence')], true);
  const outcome = await submitAnswer(stuck.ports);
  assert.equal(outcome.ok, false);
  assert.equal(stuck.log.sent.length, 8, 'a stuck race gives up after eight requests');
}

// A refusal while it still waits is shown as it is, once.
{
  const { ports, log } = world([refused('forbidden')], true);
  const outcome = await submitAnswer(ports);
  assert.equal(outcome.ok, false);
  assert.equal(outcome.error.response.code, 'forbidden');
  assert.deepEqual(log.sent, ['action/1']);
}

// If st cannot say whether it still waits, the failure stands (nothing is claimed).
{
  const { ports } = world([new TypeError('Network request failed')], new Error('offline'));
  assert.equal((await submitAnswer(ports)).ok, false);
  const unsure = world([refused('forbidden')], null);
  assert.equal((await submitAnswer(unsure.ports)).ok, false);
}

// A request that cannot even be built is reported, not sent.
{
  const { ports, log } = world(['ok'], true);
  ports.build = async () => { throw new Error('Still connecting; try again in a moment.'); };
  const outcome = await submitAnswer(ports);
  assert.equal(outcome.ok, false);
  assert.equal(log.sent.length, 0);
}
console.log('answer ok');
