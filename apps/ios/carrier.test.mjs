import assert from 'node:assert/strict';
import { DIAL_BUDGET_MS, FabricCarrier, REDIAL_WAITS_MS, REMEMBER_FAILURE_MS, routeLabel, selectRoute } from './carrier.ts';

const node = 'ab'.repeat(32);
const target = { node, service: 'st3-client/demo' };
const tail = 'http://100.64.1.2:8443', lan = 'http://host.local:8443';
const closed = { phase: 'off', bridgeUrl: '', path: 'unknown', reason: '' };

// Ports whose dial the test settles by hand, a clock it moves, and waits it releases.
function harness() {
  const log = [];
  const state = { clock: 0, dials: [], waits: [], refused: false, stops: 0 };
  const ports = {
    dial: t => new Promise((resolve, reject) => { log.push('dial'); state.dials.push({ t, resolve, reject }); }),
    stop: async () => { state.stops++; log.push('stop'); },
    refused: async () => state.refused,
    now: () => state.clock,
    wait: ms => new Promise(resolve => { state.waits.push({ ms, resolve }); }),
  };
  const seen = [];
  const carrier = new FabricCarrier(ports, snapshot => seen.push(snapshot.phase));
  const flush = async () => { for (let i = 0; i < 10; i++) await Promise.resolve(); };
  return { carrier, state, seen, log, flush };
}

// ---- Route selection: the saved gateway is the default and is named.
assert.deepEqual(selectRoute('tailscale', true, tail, closed, false), { baseUrl: tail, route: { kind: 'tailnet', fellBack: false, why: '' }, pending: false, issue: '' });
assert.equal(selectRoute('tailscale', true, lan, closed, true).route.kind, 'lan');
assert.equal(selectRoute('tailscale', true, 'https://m.example', closed, true).route.kind, 'https');
assert.equal(selectRoute('tailscale', true, null, closed, false).baseUrl, null);

// With fabric chosen: nothing is dialed over the saved route while fabric opens, the bridge when ready,
// and the saved route, named as a fallback with its reason, when fabric cannot be used.
assert.deepEqual(selectRoute('fabric', true, tail, { ...closed, phase: 'dialing' }, true), { baseUrl: null, route: { kind: 'none', fellBack: false, why: '' }, pending: true, issue: '' });
const ready = { phase: 'ready', bridgeUrl: 'http://127.0.0.1:50123', path: 'direct', reason: '' };
assert.deepEqual(selectRoute('fabric', true, tail, ready, true), { baseUrl: ready.bridgeUrl, route: { kind: 'fabric', path: 'direct', fellBack: false, why: '' }, pending: false, issue: '' });
const failed = { phase: 'failed', bridgeUrl: '', path: 'unknown', reason: 'Fabric did not answer within 4 s' };
const fell = selectRoute('fabric', true, tail, failed, true);
assert.equal(fell.baseUrl, tail);
assert.deepEqual(fell.route, { kind: 'tailnet', fellBack: true, why: failed.reason });
assert.equal(routeLabel(fell.route), 'Tailscale, because fabric could not be used');
const refused = { ...failed, phase: 'refused', reason: 'The member refused fabric access.' };
assert.equal(selectRoute('fabric', true, tail, refused, true).route.why, refused.reason, 'a refusal is named, not hidden');
// No fallback chosen, or no saved gateway: the reason is the issue, and nothing is dialed.
assert.deepEqual(selectRoute('fabric', false, tail, failed, true), { baseUrl: null, route: { kind: 'none', fellBack: false, why: '' }, pending: false, issue: failed.reason });
assert.equal(selectRoute('fabric', true, null, failed, true).baseUrl, null);
// Fabric chosen with no saved target: the saved gateway, named, never a dead end.
assert.equal(selectRoute('fabric', true, tail, closed, false).route.fellBack, true);
assert.equal(routeLabel({ kind: 'fabric', path: 'relay', fellBack: false, why: '' }), 'Fabric (relay)');
assert.equal(routeLabel({ kind: 'none', fellBack: false, why: '' }), 'Not connected');

// ---- Lifecycle: a fresh bridge each foreground, none in the background.
{
  const { carrier, state, seen, flush } = harness();
  carrier.configure(true, target);
  await flush();
  assert.equal(carrier.snapshot.phase, 'idle', 'chosen but in the background: nothing dialed');
  assert.equal(state.dials.length, 0);
  carrier.foreground(true);
  await flush();
  assert.equal(carrier.snapshot.phase, 'dialing');
  state.dials[0].resolve({ url: 'http://127.0.0.1:50001', path: 'relay' });
  await flush();
  assert.deepEqual({ ...carrier.snapshot }, { phase: 'ready', bridgeUrl: 'http://127.0.0.1:50001', path: 'relay', reason: '' });
  carrier.foreground(false);
  await flush();
  assert.equal(carrier.snapshot.phase, 'idle');
  assert.equal(carrier.snapshot.bridgeUrl, '', 'the listener ends with the foreground');
  assert.ok(state.stops >= 1);
  carrier.foreground(true);
  await flush();
  assert.equal(state.dials.length, 2, 'a new listener for the new foreground');
  state.dials[1].resolve({ url: 'http://127.0.0.1:50002' });
  await flush();
  assert.equal(carrier.snapshot.bridgeUrl, 'http://127.0.0.1:50002');
  // Turning fabric off, or removing the target, stops it.
  carrier.configure(false, target);
  await flush();
  assert.equal(carrier.snapshot.phase, 'off');
  assert.ok(seen.includes('ready'));
}

// ---- A dial that never answers gives the saved route its turn, and is not waited on again at once.
{
  const { carrier, state, flush } = harness();
  carrier.foreground(true);
  carrier.configure(true, target);
  await flush();
  assert.equal(state.dials.length, 1);
  assert.equal(state.waits.at(-1).ms, DIAL_BUDGET_MS);
  state.waits.at(-1).resolve();
  await flush();
  assert.equal(carrier.snapshot.phase, 'failed');
  assert.match(carrier.snapshot.reason, /did not answer within 4 s/);
  // A late answer from the abandoned dial changes nothing.
  state.dials[0].resolve({ url: 'http://127.0.0.1:50009' });
  await flush();
  assert.equal(carrier.snapshot.phase, 'failed');
  // The next foreground, soon after, goes straight to the saved route: no second wait.
  carrier.foreground(false); await flush();
  state.clock += REMEMBER_FAILURE_MS - 1;
  carrier.foreground(true); await flush();
  assert.equal(state.dials.length, 1);
  assert.equal(carrier.snapshot.phase, 'failed');
  // Later it tries again; and the person can ask at once.
  carrier.foreground(false); await flush();
  state.clock += 2;
  carrier.foreground(true); await flush();
  assert.equal(state.dials.length, 2);
  state.dials[1].reject(new Error('peer unreachable'));
  await flush();
  assert.equal(carrier.snapshot.reason, 'peer unreachable');
  carrier.retry(); await flush();
  assert.equal(state.dials.length, 3, 'asking again does not wait out the memory');
}

// ---- The feed over the bridge fails: a refusal ends the trial, anything else is redialed a few times.
{
  const { carrier, state, flush } = harness();
  carrier.foreground(true);
  carrier.configure(true, target);
  await flush();
  state.dials[0].resolve({ url: 'http://127.0.0.1:50001' });
  await flush();
  state.refused = true;
  await carrier.feedFailed();
  assert.equal(carrier.snapshot.phase, 'refused');
  assert.match(carrier.snapshot.reason, /refused fabric access/);
}
{
  const { carrier, state, flush } = harness();
  carrier.foreground(true);
  carrier.configure(true, target);
  await flush();
  state.dials[0].resolve({ url: 'http://127.0.0.1:50001' });
  await flush();
  for (let attempt = 0; attempt < REDIAL_WAITS_MS.length; attempt++) {
    const failing = carrier.feedFailed();
    await flush();
    assert.equal(carrier.snapshot.phase, 'dialing', `redial ${attempt + 1} after its wait`);
    const redialWait = state.waits.at(-1);
    assert.equal(redialWait.ms, REDIAL_WAITS_MS[attempt]);
    redialWait.resolve();
    await failing; await flush();
    state.dials.at(-1).resolve({ url: `http://127.0.0.1:5010${attempt}` });
    await flush();
    assert.equal(carrier.snapshot.phase, 'ready');
  }
  await carrier.feedFailed();
  assert.equal(carrier.snapshot.phase, 'failed', 'after the redials, fabric is given up for now');
  // A good connection starts the count over.
  carrier.retry(); await flush();
  state.dials.at(-1).resolve({ url: 'http://127.0.0.1:50200' });
  await flush();
  carrier.feedLive();
  const again = carrier.feedFailed(); await flush();
  assert.equal(carrier.snapshot.phase, 'dialing');
  state.waits.at(-1).resolve(); await again;
}
