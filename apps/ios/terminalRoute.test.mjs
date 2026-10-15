import assert from 'node:assert/strict';
import { test } from 'node:test';
import { Feed } from './feed.ts';
import { ForegroundGate } from './foreground.ts';
import { TerminalRouteMemory } from '@smalltalk/st3-views';
import { fakeClient } from './fakeFeed.mjs';
const id = 'terminal/agent/demo/worker';
const descriptor = { terminal_id: id, runtime_incarnation: 'runtime:one', owner_host_id: 'host/owner-a', fabric: { node: 'a'.repeat(64), service: 'st3/client/demo-fleet' } };
const settle = (ms = 15) => new Promise(resolve => setTimeout(resolve, ms));
const failure = (message, code) => Object.assign(new Error(message), code ? { response: { code } } : {});
function harness({ refused, openError, built = true, fallback = true, first = true, describeError, open, attemptMs = 1000, ownerControl = true } = {}) {
  function terminalClient(host) {
    const { client, sockets } = fakeClient();
    const calls = { reads: 0, attaches: [], detaches: [], inputs: [], resizes: [], incarnation: 'runtime:one' };
    const screen = () => ({ kind: 'terminal-screen', terminal_id: id, runtime_incarnation: calls.incarnation, lines: [], next_sequence: calls.reads });
    client.capabilities = async () => ({ value: { capabilities: [{ id: 'terminal.input', state: ownerControl ? 'granted' : 'denied' }] } });
    client.terminalScreen = async () => ({ snapshot: { id: `snapshot/${host}/${++calls.reads}` }, value: screen() });
    client.terminalAttach = async input => { calls.attaches.push(input); if (host === 'owner' && refused) throw refused; return { value: { terminal_attachment: { attachment_id: `terminal-attachment/${host}/${input.id}`, runtime_incarnation: calls.incarnation, stream_capability: `cap/${host}` } } }; };
    client.terminalDetach = async input => { calls.detaches.push(input); return { value: {} }; };
    client.terminalInput = async input => { calls.inputs.push(input); return { value: {} }; };
    client.terminalResize = async input => { calls.resizes.push(input); return { value: {} }; };
    const create = client.collectionStream;
    client.collectionStream = async options => {
      const socket = await create(options);
      if (host === 'owner') { const subscribe = socket.subscribeTerminal; socket.subscribeTerminal = (...args) => { subscribe(...args); if (first) queueMicrotask(() => socket.frame({ kind: 'screen', id: 'terminal', collection: 'terminal', snapshot: { id: 'snapshot/owner/frame' }, value: screen() })); }; }
      return socket;
    };
    return { client, sockets, calls, screen };
  }
  const gateway = terminalClient('gateway'), owner = terminalClient('owner');
  const state = { opens: 0, releases: 0, describes: 0, now: 0, built, fallback };
  const gate = new ForegroundGate('active');
  const routing = {
    built: () => state.built, fallback: () => state.fallback, memory: new TerminalRouteMemory(() => state.now), attemptMs,
    describe: async () => { state.describes++; if (describeError) throw describeError; return descriptor; },
    open: async (_route, signal) => { state.opens++; if (openError) throw openError; if (open) await open(signal); return { client: owner.client, cleanup: owner.client, release: async () => { state.releases++; }, failure: async error => ({ reason: error.message, unreachable: !error.response }) }; },
  };
  const feed = new Feed(gateway.client, { onWindow() {}, onConnection() {} }, gate, () => `action/${Math.random()}`, [5], 1000, false, 30_000, undefined, routing);
  const screens = [], issues = [], routes = [], ready = [], quality = [], controls = [];
  const follow = () => feed.followTerminal(id, { onScreen: screen => screens.push(screen.runtime_incarnation), onIssue: issue => issues.push(issue), onRoute: route => routes.push(route), onReady: value => ready.push(value), onQuality: event => quality.push(event), onControl: allowed => controls.push(allowed) });
  const subscriptions = socket => socket.sent.filter(frame => frame.collection === 'terminal');
  return { feed, gateway, owner, state, gate, follow, screens, issues, routes, ready, quality, controls, subscriptions };
}
test('owner capability, snapshot and socket stay local; gateway windows stay connected', async () => {
  const h = harness(); h.follow(); await settle();
  assert.equal(h.routes.at(-1), 'Fabric to owner-a'); assert.deepEqual(h.subscriptions(h.gateway.sockets[0]), []);
  assert.equal(h.subscriptions(h.owner.sockets[0])[0].capability, 'cap/owner');
  const control = h.feed.terminalControl(id, 'runtime:one'); assert.equal(control.client, h.owner.client); assert.equal(control.snapshot, 'snapshot/owner/frame');
  assert.throws(() => h.feed.terminalControl(id, 'runtime:two'), /restarted/);
  assert.equal(h.controls.at(-1), true);
  h.gateway.sockets[0].drop(new Error('gateway changed')); await settle(); assert.equal(h.controls.at(-1), true, 'the owner grant survives loss of gateway capabilities'); assert.equal(h.state.releases, 0); assert.equal(h.owner.calls.attaches.length, 1); assert.equal(h.ready.at(-1), true);
  h.feed.close(); await settle(); assert.equal(h.owner.calls.detaches.length, 1); assert.equal(h.owner.calls.detaches[0].parameters.target_id, `terminal-attachment/owner/${h.owner.calls.attaches[0].id}`); assert.equal(h.gateway.calls.detaches.length, 0); assert.equal(h.state.releases, 1);
  assert.throws(() => h.feed.terminalControl(id, 'runtime:one'), /not ready/);
  assert.deepEqual(h.quality, ['attempt', 'attached'], 'gateway loss and intentional close do not count a healthy owner as dead');
});
test('refusal falls back visibly with fresh gateway capability and unchanged pin', async () => {
  const h = harness({ refused: failure('owner refused read scope', 'forbidden') }); h.follow(); await settle();
  assert.deepEqual(h.quality, ['attempt', 'error', 'attempt', 'attached']);
  assert.match(h.routes.at(-1), /via the daemon.*owner refused read scope/); assert.equal(h.gateway.calls.attaches[0].fence.runtime_incarnation, 'runtime:one');
  assert.equal(h.subscriptions(h.gateway.sockets[0])[0].capability, 'cap/gateway');
  h.gateway.sockets[0].frame({ kind: 'screen', id: 'terminal', collection: 'terminal', snapshot: { id: 'snapshot/gateway/frame' }, value: h.gateway.screen() });
  assert.match(h.routes.at(-1), /owner refused read scope/); assert.equal(h.feed.terminalControl(id, 'runtime:one').snapshot, 'snapshot/gateway/frame');
  h.follow(); await settle(); assert.equal(h.state.opens, 2, 'terminal refusal does not poison owner memory'); h.feed.close();
});
test('late gateway frames cannot ready or fence an owner before its first screen', async () => {
  const h = harness({ first: false }); h.follow(); await settle();
  assert.equal(h.owner.calls.attaches.length, 1);
  assert.throws(() => h.feed.terminalControl(id, 'runtime:one'), /not ready/);
  h.gateway.sockets[0].frame({ kind: 'screen', id: 'terminal', collection: 'terminal', snapshot: { id: 'snapshot/gateway/late' }, value: h.gateway.screen() });
  h.gateway.sockets[0].frame({ kind: 'error', id: 'terminal', code: 'internal', message: 'old gateway follow failed' });
  assert.equal(h.ready.at(-1), false);
  assert.throws(() => h.feed.terminalControl(id, 'runtime:one'), /not ready/);
  h.owner.sockets[0].frame({ kind: 'screen', id: 'terminal', collection: 'terminal', snapshot: { id: 'snapshot/owner/first' }, value: h.owner.screen() });
  await settle();
  const control = h.feed.terminalControl(id, 'runtime:one');
  assert.equal(control.client, h.owner.client); assert.equal(control.snapshot, 'snapshot/owner/first');
  assert.equal(h.ready.at(-1), true); assert.equal(h.routes.at(-1), 'Fabric to owner-a');
  h.feed.close();
});
for (const reason of ['dial failed', 'missing peer grant', 'tunnel failed']) test(`${reason}: only the actual failure starts the minute`, async () => {
  const h = harness({ openError: failure(reason) }); h.follow(); await settle(); assert.match(h.routes.at(-1), new RegExp(reason));
  h.state.now = 59_999; h.follow(); await settle(); assert.equal(h.state.opens, 1); assert.match(h.routes.at(-1), /not tried again yet/);
  h.state.now = 60_000; h.follow(); await settle(); assert.equal(h.state.opens, 2); h.feed.close();
});
for (const options of [{ built: false }, { refused: failure('owner refusal', 'forbidden') }, { describeError: failure('older daemon has no route', 'not-found') }, { openError: failure('missing grant') }]) test(`fabric-only never attaches through gateway (${JSON.stringify(options)})`, async () => {
  const h = harness({ ...options, fallback: false }); h.follow(); await settle(); assert.equal(h.gateway.calls.attaches.length, 0); assert.match(h.issues.at(-1), /fabric only, no fallback/); h.feed.close();
});
test('older daemon and build without Fabric use gateway by default', async () => {
  for (const options of [{ built: false }, { describeError: failure('older daemon has no route', 'not-found') }]) { const h = harness(options); h.follow(); await settle(); assert.equal(h.gateway.calls.attaches.length, 1); assert.equal(h.state.opens, 0); h.feed.close(); }
});
test('foreground reattachment rejects a replacement incarnation', async () => {
  const h = harness(); h.follow(); await settle(); h.gate.update('background'); await settle();
  h.gateway.calls.incarnation = 'runtime:two'; h.owner.calls.incarnation = 'runtime:two'; h.gate.update('active'); await settle();
  assert.match(h.issues.at(-1), /Terminal restarted/); assert.equal(h.owner.calls.attaches.length, 1); assert.equal(h.gateway.calls.attaches.length, 0); h.feed.close();
});
test('complete attempt budget includes first stream screen and cancels late work', async () => {
  const h = harness({ first: false, attemptMs: 20 }); h.follow(); await settle(50);
  assert.match(h.routes.at(-1), /did not answer within 0.02 s/); assert.equal(h.gateway.calls.attaches.length, 1); assert.equal(h.state.releases, 1); assert.equal(h.owner.sockets[0].closed, true);
  h.owner.sockets[0].frame({ kind: 'screen', id: 'terminal', collection: 'terminal', snapshot: { id: 'late' }, value: h.owner.screen() }); assert.match(h.routes.at(-1), /via the daemon/); h.feed.close();
});
test('cancelled late dial releases its target without publishing a route', async () => {
  let resolve; const h = harness({ open: () => new Promise(done => { resolve = done; }) }); const follow = h.follow(); await settle(); follow.close(); resolve(); await settle();
  assert.equal(h.owner.calls.attaches.length, 0); assert.equal(h.state.releases, 1); assert.deepEqual(h.routes, []); h.feed.close();
});
test('unconfirmed control is not replayed and later input selects the fallback client', async () => {
  const h = harness(); h.follow(); await settle(); h.feed.terminalControlFailed(failure('response lost')); await settle();
  assert.equal(h.owner.calls.inputs.length + h.gateway.calls.inputs.length, 0); assert.equal(h.feed.terminalControl(id, 'runtime:one').client, h.gateway.client); assert.match(h.routes.at(-1), /via the daemon.*response lost/); h.feed.close();
});

test('a local listener failure is visible and never remembered as owner unreachability', async () => {
  const h = harness({ openError: Object.assign(failure('loopback listener could not bind'), { ownerUnreachable: false }) });
  h.follow(); await settle(); assert.match(h.routes.at(-1), /loopback listener could not bind/);
  h.follow(); await settle(); assert.equal(h.state.opens, 2); h.feed.close();
});

test('a gateway local failure remains a named error rather than a reconnect loop', async () => {
  for (const code of ['internal', 'terminal-unavailable']) {
    const h = harness({ refused: failure('owner refused this terminal', 'forbidden') });
    let reads = 0; h.gateway.client.terminalScreen = async () => { reads++; throw failure('gateway PTY path is broken', code); };
    h.follow(); await settle(40); assert.equal(reads, 1); assert.equal(h.gateway.calls.attaches.length, 0);
    assert.match(h.issues.at(-1), /gateway PTY path is broken/); h.feed.close();
  }
});

test('routed quality counts owner loss once and excludes background suspension', async () => {
  const h = harness(); h.follow(); await settle();
  h.owner.sockets[0].drop(new Error('owner disconnected')); await settle();
  assert.deepEqual(h.quality, ['attempt', 'attached', 'dead', 'attempt', 'attached']);
  h.gate.update('background');
  h.feed.close();
  assert.equal(h.quality.filter(event => event === 'dead').length, 1);
});

test('an owner read-only grant disables controls independently of gateway capabilities', async () => {
  const h = harness({ ownerControl: false }); h.follow(); await settle();
  assert.equal(h.ready.at(-1), true);
  assert.equal(h.controls.at(-1), false);
  h.gateway.sockets[0].drop(new Error('gateway disconnected')); await settle();
  assert.equal(h.controls.at(-1), false);
  h.feed.close(); assert.equal(h.controls.at(-1), undefined);
});
