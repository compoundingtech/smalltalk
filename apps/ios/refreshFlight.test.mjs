import assert from 'node:assert/strict';
import { RefreshFlight } from './refreshFlight.ts';

const flight = new RefreshFlight();
assert.equal(flight.start(0), true);
assert.equal(flight.start(0), false);
assert.equal(flight.start(1), true, 'a new credential generation must not wait for the old request');
flight.finish(0);
assert.equal(flight.isCurrent(1), true, 'the old request must not clear the new flight');
assert.equal(flight.start(1), false);
flight.finish(1);
assert.equal(flight.start(1), true);

// Event-driven full reloads are coalesced: a burst of projection events causes at most one
// reload per interval, with the remaining wait returned so the caller sleeps instead of reloading.
const { coalescedRefreshDelay } = await import('./refreshFlight.ts');
assert.equal(coalescedRefreshDelay(0, 1_000, 10_000), 0, 'the first event-driven reload is not delayed');
assert.equal(coalescedRefreshDelay(100_000, 103_000, 10_000), 7_000);
assert.equal(coalescedRefreshDelay(100_000, 110_000, 10_000), 0);
let reloads = 0, last = 0;
for (let at = 1; at <= 60_000; at += 250) if (coalescedRefreshDelay(last, at, 10_000) === 0) { reloads++; last = at; }
assert.ok(reloads <= 6, `a minute of back-to-back events reloaded ${reloads} times`);
