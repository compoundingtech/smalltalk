import assert from 'node:assert/strict';
import { test } from 'node:test';
import { FabricSession, nativeRefusedAfter } from './fabricSession.ts';

const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; };
const fixture = overrides => {
  let dials = 0, pairs = 0, stops = 0, state;
  const session = new FabricSession({
    dial: async () => ({ url: `http://127.0.0.1:${12000 + ++dials}` }),
    pair: async () => { pairs++; return 'private-test-credential'; },
    authenticate: async () => 'person/demo', stop: async () => { stops++; }, ...overrides,
  }, value => { state = value; });
  return { session, get state() { return state; }, get counts() { return { dials, pairs, stops }; } };
};

test('background closes the listener; foreground authenticates a new listener without pairing again', async () => {
  const f = fixture();
  await f.session.foreground(true);
  const first = f.state.url;
  assert.equal(f.state.ready, true);
  await f.session.foreground(false);
  assert.equal(f.state.ready, false);
  assert.equal(f.counts.stops, 1);
  await f.session.foreground(true);
  assert.notEqual(f.state.url, first);
  assert.equal(f.state.ready, true);
  assert.equal(f.counts.pairs, 1);
  assert.equal(JSON.stringify(f.state.events).includes('private-test-credential'), false);
  await f.session.close();
});

test('a pairing reply received while backgrounded is retained without reusing the one-time challenge', async () => {
  const paired = deferred(), entered = deferred();
  let pairs = 0;
  const f = fixture({ pair: async () => { pairs++; entered.resolve(); return paired.promise; } });
  const first = f.session.foreground(true);
  await entered.promise;
  await f.session.foreground(false);
  const resume = f.session.foreground(true);
  paired.resolve('private-test-credential');
  await Promise.all([first, resume]);
  assert.equal(pairs, 1);
  assert.equal(f.state.ready, true);
  assert.equal(f.counts.dials, 2);
  await f.session.close();
});

test('an unanswered pairing is never automatically replayed', async () => {
  let pairs = 0;
  const f = fixture({ pair: async () => { pairs++; throw new Error('answer lost'); } });
  await f.session.foreground(true);
  assert.equal(f.state.ready, false);
  await f.session.foreground(false);
  await f.session.foreground(true);
  assert.equal(pairs, 1);
  assert.match(f.state.issue, /fresh proof pairing link/);
  await f.session.close();
});

test('closing while dial is pending never pairs or publishes a usable profile', async () => {
  const dialing = deferred(), entered = deferred();
  const f = fixture({ dial: async () => { entered.resolve(); return dialing.promise; } });
  const opening = f.session.foreground(true);
  await entered.promise;
  await f.session.close();
  dialing.resolve({ url: 'http://127.0.0.1:12000' });
  await opening;
  assert.equal(f.counts.pairs, 0);
  assert.equal(f.state.ready, false);
});

test('a different actor on resume cannot become a live proof session', async () => {
  let reads = 0;
  const f = fixture({ authenticate: async () => ++reads === 1 ? 'person/demo' : 'person/other' });
  await f.session.foreground(true);
  await f.session.foreground(false);
  await f.session.foreground(true);
  assert.equal(f.state.ready, false);
  assert.match(f.state.issue, /actor changed/);
  for (let n = 0; n < 200; n++) f.session.record('sample');
  assert.equal(f.state.events.length, 128);
  await f.session.close();
});

test('admission refusal stops the live session and forbids automatic redial on resume', async () => {
  const f = fixture();
  await f.session.foreground(true);
  await f.session.refuse();
  assert.equal(f.state.ready, false);
  assert.match(f.state.issue, /member refused/);
  await f.session.foreground(false);
  await f.session.foreground(true);
  assert.equal(f.counts.dials, 1);
  assert.equal(f.counts.pairs, 1);
  await f.session.close();
});

test('native refusals are scoped to attempts after this trial began', () => {
  assert.equal(nativeRefusedAfter({ attempts: [{ id: 2, result: 'refused' }, { id: 3, result: 'connected' }] }, 2), false);
  assert.equal(nativeRefusedAfter({ attempts: [{ id: 3, result: 'refused' }] }, 2), true);
  assert.equal(nativeRefusedAfter({ attempts: [{ id: '3', result: 'refused' }] }, 2), false);
  assert.equal(nativeRefusedAfter({}, 0), false);
});
