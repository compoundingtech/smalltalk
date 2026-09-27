import assert from 'node:assert/strict';
import { withFreshTerminalFence } from './terminalControls.ts';

const terminalId = 'terminal/worker';
const screen = (index, incarnation = 'incarnation/one') => ({
  snapshot: { id: `snapshot/host/${index}/proof` },
  value: { runtime_incarnation: incarnation, next_sequence: index },
});

let reads = 0;
const sent = [];
const result = await withFreshTerminalFence(
  { terminalScreen: async id => { assert.equal(id, terminalId); return screen(++reads); } },
  terminalId,
  'incarnation/one',
  async fence => {
    sent.push(fence);
    if (sent.length === 1) throw { response: { code: 'stale-fence' } };
    return 'sent';
  },
);
assert.equal(result, 'sent');
assert.equal(reads, 2);
assert.deepEqual(sent.map(fence => [fence.snapshot_id, fence.terminal_sequence]), [
  ['snapshot/host/1/proof', 1], ['snapshot/host/2/proof', 2],
]);
assert.ok(sent.every(fence => fence.runtime_incarnation === 'incarnation/one'));

let acted = false;
await assert.rejects(withFreshTerminalFence(
  { terminalScreen: async () => screen(3, 'incarnation/two') }, terminalId, 'incarnation/one',
  async () => { acted = true; },
), /restarted/);
assert.equal(acted, false);

let attempts = 0;
await assert.rejects(withFreshTerminalFence(
  { terminalScreen: async () => screen(++attempts) }, terminalId, 'incarnation/one',
  async () => { throw { response: { code: 'stale-fence' } }; },
), error => error.response.code === 'stale-fence');
assert.equal(attempts, 3);

let nonStaleAttempts = 0;
await assert.rejects(withFreshTerminalFence(
  { terminalScreen: async () => screen(++nonStaleAttempts) }, terminalId, 'incarnation/one',
  async () => { throw new Error('offline'); },
), /offline/);
assert.equal(nonStaleAttempts, 1);
