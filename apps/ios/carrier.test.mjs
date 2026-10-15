import assert from 'node:assert/strict';
import { DIAL_BUDGET_MS, FabricCarrier, FabricRefusal, REDIAL_WAITS_MS, RETRY_WAITS_MS, REMEMBER_FAILURE_MS, WALK_BUDGET_MS, refusalText, routeLabel, selectRoute } from './carrier.ts';

const node = 'ab'.repeat(32);
const target = { node, service: 'st3-client/demo', name: 'member 1' };
// A list of one: today's single saved target.
const one = [target];
const tail = 'http://100.64.1.2:8443', lan = 'http://host.local:8443';
const closed = { phase: 'off', bridgeUrl: '', path: 'unknown', reason: '', member: '', tried: 0 };

// Ports whose dial the test settles by hand, a clock it moves, and waits it releases.
function harness() {
  const log = [];
  const state = { clock: 0, dials: [], waits: [], refused: null, stops: 0 };
  const ports = {
    dial: t => new Promise((resolve, reject) => { log.push('dial'); state.dials.push({ t, resolve, reject }); }),
    stop: async () => { state.stops++; log.push('stop'); },
    refusal: async () => state.refused,
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
  carrier.configure(true, one);
  await flush();
  assert.equal(carrier.snapshot.phase, 'idle', 'chosen but in the background: nothing dialed');
  assert.equal(state.dials.length, 0);
  carrier.foreground(true);
  await flush();
  assert.equal(carrier.snapshot.phase, 'dialing');
  state.dials[0].resolve({ url: 'http://127.0.0.1:50001', path: 'relay' });
  await flush();
  assert.deepEqual({ ...carrier.snapshot }, { phase: 'ready', bridgeUrl: 'http://127.0.0.1:50001', path: 'relay', reason: '', member: 'member 1', tried: 1 });
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
  carrier.configure(false, one);
  await flush();
  assert.equal(carrier.snapshot.phase, 'off');
  assert.ok(seen.includes('ready'));
}

// ---- A dial that never answers gives the saved route its turn, and is not waited on again at once.
{
  const { carrier, state, flush } = harness();
  carrier.foreground(true);
  carrier.configure(true, one);
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
  carrier.configure(true, one);
  await flush();
  state.dials[0].resolve({ url: 'http://127.0.0.1:50001' });
  await flush();
  state.refused = new FabricRefusal('peer_not_permitted_for_service');
  await carrier.feedFailed();
  assert.equal(carrier.snapshot.phase, 'refused');
  assert.equal(carrier.snapshot.reason, refusalText('member 1', 'peer_not_permitted_for_service'));
  assert.equal(carrier.snapshot.member, 'member 1');
}
{
  const { carrier, state, flush } = harness();
  carrier.foreground(true);
  carrier.configure(true, one);
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

// The fabric-only setting also governs an absent target, before any network attempt.
{
  const selected = selectRoute('fabric', false, 'https://gateway.example', { phase: 'off' }, false);
  assert.equal(selected.baseUrl, null);
  assert.match(selected.issue, /No fabric target/);
}

// ---- A failed fabric is tried again by itself, so a phone that fell back to a saved route it cannot reach
// ---- does not stay there until it is killed (Nathan, 2026-10-13: "reconnecting over and over ... killed it, it connected").
{
  const { carrier, state, flush } = harness();
  const seen = [];
  carrier.foreground(true);
  carrier.configure(true, one);
  await flush();
  state.waits.at(-1).resolve(); // the dial budget runs out
  await flush();
  assert.equal(carrier.snapshot.phase, 'failed');
  const retryWait = state.waits.at(-1);
  assert.equal(retryWait.ms, RETRY_WAITS_MS[0], 'the first try again is after the memory of a failure');
  state.clock += RETRY_WAITS_MS[0];
  retryWait.resolve(); await flush();
  assert.equal(state.dials.length, 2, 'fabric is dialed again with no foreground change and no tap');
  state.dials[1].reject(new Error('peer unreachable')); await flush();
  assert.equal(carrier.snapshot.phase, 'failed');
  assert.equal(state.waits.at(-1).ms, RETRY_WAITS_MS[1], 'each failure waits longer');
  state.clock += RETRY_WAITS_MS[1];
  state.waits.at(-1).resolve(); await flush();
  assert.equal(state.dials.length, 3);
  state.dials[2].resolve({ url: 'http://127.0.0.1:50300' }); await flush();
  assert.equal(carrier.snapshot.phase, 'ready', 'and when it answers the route is fabric again');
  void seen;
}
{
  // Leaving the foreground, or turning fabric off, cancels the pending try.
  const { carrier, state, flush } = harness();
  carrier.foreground(true);
  carrier.configure(true, one);
  await flush();
  state.waits.at(-1).resolve(); await flush();
  const retryWait = state.waits.at(-1);
  carrier.configure(false, one); await flush();
  state.clock += RETRY_WAITS_MS[0];
  retryWait.resolve(); await flush();
  assert.equal(state.dials.length, 1, 'a cancelled try dials nothing');
  assert.equal(carrier.snapshot.phase, 'off');
}

// ---- Several members: walked in the person's order, each within its own limit and all within the walk's.
const memberNamed = (letter, name) => ({ node: letter.repeat(64), service: 'st3-client/demo', name });
const home = memberNamed('a', 'home'), office = memberNamed('b', 'office'), spare = memberNamed('c', 'spare'), last = memberNamed('d', 'last');
const dialed = state => state.dials.map(dial => dial.t.name);

{
  // The first member is down and the second answers.
  const { carrier, state, flush } = harness();
  carrier.foreground(true);
  carrier.configure(true, [home, office]);
  await flush();
  assert.deepEqual(dialed(state), ['home']);
  assert.equal(state.waits.at(-1).ms, DIAL_BUDGET_MS);
  state.dials[0].reject(new Error('peer unreachable'));
  await flush();
  assert.deepEqual(dialed(state), ['home', 'office'], 'a member that cannot be reached gives the next its turn');
  assert.equal(carrier.snapshot.phase, 'dialing');
  state.dials[1].resolve({ url: 'http://127.0.0.1:50401', path: 'direct' });
  await flush();
  assert.equal(carrier.snapshot.phase, 'ready');
  assert.equal(carrier.snapshot.member, 'office');
  assert.equal(carrier.snapshot.tried, 2, 'how many members were tried is kept, as a count');
}
{
  // Every member is down: each dial has its 4 s and the walk its 10 s in all, so the third gets what is left and the fourth none.
  const { carrier, state, flush } = harness();
  carrier.foreground(true);
  carrier.configure(true, [home, office, spare, last]);
  await flush();
  const budgets = [];
  for (let i = 0; i < 3; i++) {
    const budget = state.waits.at(-1);
    budgets.push(budget.ms);
    state.clock += budget.ms;
    budget.resolve();
    await flush();
  }
  assert.deepEqual(budgets, [DIAL_BUDGET_MS, DIAL_BUDGET_MS, WALK_BUDGET_MS - 2 * DIAL_BUDGET_MS]);
  assert.deepEqual(dialed(state), ['home', 'office', 'spare'], 'the total limit holds: the fourth is not dialed');
  assert.equal(carrier.snapshot.phase, 'failed');
  assert.equal(carrier.snapshot.tried, 3);
  assert.match(carrier.snapshot.reason, /within 10 s \(3 of 4 tried\)/);
  assert.equal(state.waits.at(-1).ms, RETRY_WAITS_MS[0], 'a failed walk is tried again later, as a failed member is');
  // The saved gateway takes over exactly as it does today.
  assert.equal(selectRoute('fabric', true, tail, carrier.snapshot, true).baseUrl, tail);
}
{
  // All down, each failing at once: the reason says none answered.
  const { carrier, state, flush } = harness();
  carrier.foreground(true);
  carrier.configure(true, [home, office]);
  await flush();
  state.dials[0].reject(new Error('peer unreachable')); await flush();
  state.dials[1].reject(new Error('peer unreachable')); await flush();
  assert.equal(carrier.snapshot.reason, 'None of the 2 members answered over fabric');
}
{
  // A refusal on the first member: nothing after it is dialed, now or later, and the refuser is named in words.
  const { carrier, state, flush } = harness();
  carrier.foreground(true);
  carrier.configure(true, [home, office, spare]);
  await flush();
  state.dials[0].reject(new FabricRefusal('peer_not_in_allow_list'));
  await flush();
  assert.equal(carrier.snapshot.phase, 'refused');
  assert.equal(carrier.snapshot.member, 'home');
  assert.equal(carrier.snapshot.reason, 'home refused this phone: it has no fabric grant for this phone. Members after it are not tried after a refusal; pick one to try it.');
  assert.ok(!carrier.snapshot.reason.includes(home.node.slice(0, 8)), 'never a node ID');
  for (const wait of state.waits.splice(0)) { state.clock += wait.ms; wait.resolve(); }
  await flush();
  assert.deepEqual(dialed(state), ['home'], 'nothing walks on by itself after a refusal');
  // The saved gateway behaves as it does after a refusal today, and says why.
  assert.equal(selectRoute('fabric', true, tail, carrier.snapshot, true).route.why, carrier.snapshot.reason);
  // The person picks another member by hand: only it is dialed.
  carrier.pick(spare); await flush();
  assert.deepEqual(dialed(state), ['home', 'spare']);
  state.dials[1].resolve({ url: 'http://127.0.0.1:50501' }); await flush();
  assert.equal(carrier.snapshot.phase, 'ready');
  assert.equal(carrier.snapshot.member, 'spare');
  // Asking for the list again walks it from the top.
  carrier.retry(); await flush();
  assert.deepEqual(dialed(state), ['home', 'spare', 'home']);
}
{
  // A refusal on the second member after the first was down: the third is not dialed.
  const { carrier, state, flush } = harness();
  carrier.foreground(true);
  carrier.configure(true, [home, office, spare]);
  await flush();
  state.dials[0].reject(new Error('peer unreachable')); await flush();
  state.dials[1].reject(new FabricRefusal('peer_not_permitted_for_service')); await flush();
  assert.equal(carrier.snapshot.phase, 'refused');
  assert.equal(carrier.snapshot.member, 'office');
  assert.equal(carrier.snapshot.reason, refusalText('office', 'peer_not_permitted_for_service'));
  assert.match(carrier.snapshot.reason, /^office refused this phone: its fabric grant for this phone does not include the client service\./);
  assert.deepEqual(dialed(state), ['home', 'office']);
  // A refusal without a code (a member older than fabric 0.2.41) is still a refusal, in plain words.
  assert.match(refusalText('office', null), /^office refused this phone's fabric access\./);
}
{
  // A connected member that drops is redialed on the reconnect schedule before the walk moves on.
  const { carrier, state, flush } = harness();
  carrier.foreground(true);
  carrier.configure(true, [home, office]);
  await flush();
  state.dials[0].resolve({ url: 'http://127.0.0.1:50601' }); await flush();
  assert.equal(carrier.snapshot.member, 'home');
  const failing = carrier.feedFailed(); await flush();
  for (let attempt = 0; attempt < REDIAL_WAITS_MS.length; attempt++) {
    const wait = state.waits.at(-1);
    assert.equal(wait.ms, REDIAL_WAITS_MS[attempt], `redial ${attempt + 1} waits its turn`);
    assert.equal(carrier.snapshot.phase, 'dialing');
    wait.resolve(); await flush();
    assert.equal(state.dials.at(-1).t.name, 'home', 'the same member is redialed, not the next');
    state.dials.at(-1).reject(new Error('peer unreachable')); await flush();
  }
  await failing;
  assert.deepEqual(dialed(state), ['home', 'home', 'home', 'home', 'office'], 'then the walk moves on to the next member');
  state.dials.at(-1).resolve({ url: 'http://127.0.0.1:50602' }); await flush();
  assert.equal(carrier.snapshot.member, 'office');
  // When the last member drops and stays down, fabric is given up for now and the saved gateway takes over.
  const again = carrier.feedFailed(); await flush();
  for (let attempt = 0; attempt < REDIAL_WAITS_MS.length; attempt++) {
    state.waits.at(-1).resolve(); await flush();
    state.dials.at(-1).reject(new Error('peer unreachable')); await flush();
  }
  await again;
  assert.equal(carrier.snapshot.phase, 'failed');
  assert.equal(carrier.snapshot.reason, 'Fabric lost its connection to office');
  assert.equal(dialed(state).at(-1), 'office', 'the members before it are not walked again until the next try');
}
{
  // The person's order stands: a list edited by hand is walked in its new order, not by what worked last.
  const { carrier, state, flush } = harness();
  carrier.foreground(true);
  carrier.configure(true, [home, office]);
  await flush();
  state.dials[0].reject(new Error('peer unreachable')); await flush();
  state.dials[1].resolve({ url: 'http://127.0.0.1:50701' }); await flush();
  carrier.foreground(false); await flush();
  carrier.foreground(true); await flush();
  assert.equal(dialed(state).at(-1), 'home', 'the next foreground starts at the top again, not at the member that worked');
  carrier.configure(true, [office, home]); await flush();
  assert.equal(dialed(state).at(-1), 'office', 'a reordered list is walked in its new order at once');
  carrier.configure(true, [office]); await flush();
  assert.equal(carrier.snapshot.phase, 'dialing', 'removing a member walks what is left');
  carrier.configure(true, []); await flush();
  assert.equal(carrier.snapshot.phase, 'off');
}
{
  // Pairing a member on its own: it is not in the walked list until it is paired, and the person's pick dials it alone.
  const { carrier, state, flush } = harness();
  carrier.foreground(true);
  carrier.configure(true, [home]);
  await flush();
  state.dials[0].resolve({ url: 'http://127.0.0.1:50801' }); await flush();
  carrier.pick(office); await flush();
  assert.deepEqual(dialed(state), ['home', 'office']);
  state.dials[1].resolve({ url: 'http://127.0.0.1:50802' }); await flush();
  assert.equal(carrier.snapshot.member, 'office');
  assert.equal(carrier.picked, office);
  // Once paired it joins the list; the new list is walked in the person's order and the pick ends.
  carrier.configure(true, [home, office]); await flush();
  assert.equal(carrier.picked, null);
  assert.equal(dialed(state).at(-1), 'home');
}
