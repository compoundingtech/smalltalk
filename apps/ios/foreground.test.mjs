import assert from 'node:assert/strict';
import { ForegroundGate } from './foreground.ts';

// Background: waiting for the foreground must not schedule timers or spin.
const scheduled = [];
const realSetTimeout = globalThis.setTimeout;
globalThis.setTimeout = (callback, ms, ...rest) => { scheduled.push(ms); return realSetTimeout(callback, ms, ...rest); };
try {
  const gate = new ForegroundGate('background');
  assert.equal(gate.active, false);
  let resumed = 0;
  const waiting = [gate.untilActive().then(() => resumed++), gate.untilActive().then(() => resumed++)];
  for (let turn = 0; turn < 20; turn++) await Promise.resolve();
  assert.equal(resumed, 0);
  assert.deepEqual(scheduled, []);
  gate.update('inactive');
  for (let turn = 0; turn < 5; turn++) await Promise.resolve();
  assert.equal(resumed, 0);
  gate.update('active');
  await Promise.all(waiting);
  assert.equal(resumed, 2);
  assert.equal(gate.active, true);
  await gate.untilActive();
  assert.deepEqual(scheduled, []);

  // Foreground transitions are reported once so the app can resync on return.
  const transitions = [];
  const unsubscribe = gate.subscribe(active => transitions.push(active));
  gate.update('active');
  gate.update('background');
  gate.update('background');
  gate.update('active');
  unsubscribe();
  gate.update('background');
  assert.deepEqual(transitions, [false, true]);
} finally {
  globalThis.setTimeout = realSetTimeout;
}
