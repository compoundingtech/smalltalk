import assert from 'node:assert/strict';
import { test } from 'node:test';
import { AppUpdateSession, DAILY_APP, DAILY_CHANNEL, mintAppUpdateToken } from './appUpdates.ts';
import { ForegroundGate } from './foreground.ts';

// The native boundary is unavailable in Node; this explicit port records the observable contract.
const fixture = (overrides = {}, initialState = 'active') => {
  const events = [], consent = [];
  const foreground = new ForegroundGate(initialState);
  const port = {
    enabled: true,
    mint: async (gateway, credential) => { events.push(['mint', gateway, credential]); return { token: 'narrow-update-token', expiresAtUnixMs: 901000 }; },
    setGateway: async gateway => { events.push(['gateway', gateway]); },
    setHeaders: headers => { events.push(['headers', headers]); },
    check: async () => { events.push(['check']); return { isAvailable: true, isRollBackToEmbedded: false }; },
    fetch: async () => { events.push(['fetch']); return { isNew: true, isRollBackToEmbedded: false }; },
    reload: async () => { events.push(['reload']); },
    consent: apply => { consent.push(apply); events.push(['consent']); },
    onFailure: () => { events.push(['failure']); },
    now: () => 1000,
    ...overrides,
  };
  return { events, consent, foreground, session: new AppUpdateSession(port, foreground) };
};

const deferred = () => {
  let resolve;
  const promise = new Promise(done => { resolve = done; });
  return { promise, resolve };
};

test('no missing Keychain credential or dev build can initiate OTA', async () => {
  const daily = fixture();
  daily.session.setPairing('https://gateway.example', null);
  await daily.session.check();
  assert.deepEqual(daily.events, [['headers', null]]);
  daily.session.close();
  const dev = fixture({ enabled: false });
  dev.session.setPairing('https://gateway.example', 'paired-secret');
  await dev.session.check();
  dev.session.close();
  assert.deepEqual(dev.events, []);
});

test('mint happens before native headers, only narrow bearer reaches Expo, reload requires consent', async () => {
  const f = fixture();
  f.session.setPairing('https://gateway.example/', 'paired-secret');
  await f.session.check();
  assert.deepEqual(f.events, [
    ['headers', null], ['headers', null],
    ['mint', 'https://gateway.example', 'paired-secret'], ['gateway', 'https://gateway.example'],
    ['headers', { Authorization: 'Bearer narrow-update-token' }], ['check'], ['fetch'], ['consent'], ['headers', null],
  ]);
  assert.equal(f.events.some(event => event[0] === 'reload'), false);
  f.consent[0]();
  assert.equal(f.events.at(-1)[0], 'reload');
  f.session.close();
});

test('revocation, expiry and signature/download failure leave current bundle and clear headers', async () => {
  for (const overrides of [
    { mint: async () => { throw new Error('revoked'); } },
    { mint: async () => ({ token: 'expired', expiresAtUnixMs: 1000 }) },
    { mint: async () => ({ token: 'too-long', expiresAtUnixMs: 901001 }) },
    { fetch: async () => { throw new Error('invalid signature'); } },
  ]) {
    const f = fixture(overrides);
    f.session.setPairing('https://gateway.example', 'paired-secret');
    await f.session.check();
    assert.equal(f.consent.length, 0);
    assert.equal(f.events.some(event => event[0] === 'reload'), false);
    assert.equal(f.events.some(event => event[0] === 'failure'), true);
    assert.deepEqual(f.events.at(-1), ['headers', null]);
    f.session.close();
  }
});

test('re-pairing fences a pending mint and serializes the replacement gateway', async () => {
  const pending = deferred();
  const f = fixture({ mint: async gateway => gateway.includes('old') ? pending.promise : { token: 'replacement', expiresAtUnixMs: 901000 } });
  f.session.setPairing('https://old.example', 'old-paired');
  const running = f.session.check();
  f.session.setPairing('https://new.example', 'new-paired');
  pending.resolve({ token: 'stale', expiresAtUnixMs: 901000 });
  await running;
  assert.deepEqual(f.events.filter(event => event[0] === 'gateway'), [['gateway', 'https://new.example']]);
  assert.deepEqual(f.events.filter(event => event[0] === 'headers' && event[1]), [['headers', { Authorization: 'Bearer replacement' }]]);
  f.session.close();
});

test('background completion waits for foreground consent and never reloads in background', async () => {
  const pending = deferred(), started = deferred();
  const f = fixture({ fetch: async () => { started.resolve(); return pending.promise; } });
  f.session.setPairing('https://gateway.example', 'paired-secret');
  const running = f.session.check();
  await started.promise;
  f.foreground.update('background');
  pending.resolve({ isNew: true, isRollBackToEmbedded: false });
  await running;
  assert.equal(f.consent.length, 0);
  f.foreground.update('active');
  assert.equal(f.consent.length, 1);
  f.foreground.update('background');
  f.consent[0]();
  assert.equal(f.events.some(event => event[0] === 'reload'), false);
  f.session.close();
});

test('closing or unpairing fences consent and clears persisted header', async () => {
  for (const close of [false, true]) {
    const f = fixture();
    f.session.setPairing('https://gateway.example', 'paired-secret');
    await f.session.check();
    if (close) f.session.close(); else f.session.setPairing('', null);
    f.consent[0]();
    assert.equal(f.events.some(event => event[0] === 'reload'), false);
    assert.deepEqual(f.events.at(-1), ['headers', null]);
    f.session.close();
  }
});

test('no-update checks coalesce and the next foreground mints a fresh token', async () => {
  const f = fixture({ check: async () => ({ isAvailable: false, isRollBackToEmbedded: false }) });
  f.session.setPairing('https://gateway.example', 'paired-secret');
  await Promise.all([f.session.check(), f.session.check()]);
  assert.equal(f.events.filter(event => event[0] === 'mint').length, 1);
  f.foreground.update('background');
  f.foreground.update('active');
  await f.session.check();
  assert.equal(f.events.filter(event => event[0] === 'mint').length, 2);
  f.session.close();
});

test('mint uses paired POST only, refuses redirects and decodes standard st envelope', async () => {
  let request;
  const result = await mintAppUpdateToken('https://gateway.example', 'paired-secret', async (url, options) => {
    request = { url, options };
    return new Response(JSON.stringify({ api_version: 'st3.client.v0', value: { token: 'narrow', expiresAtUnixMs: 1234 } }));
  });
  assert.deepEqual(result, { token: 'narrow', expiresAtUnixMs: 1234 });
  assert.equal(request.url, 'https://gateway.example/v1/client/app-updates/token');
  assert.equal(request.options.redirect, 'error');
  assert.equal(request.options.headers.Authorization, 'Bearer paired-secret');
  assert.deepEqual(JSON.parse(request.options.body), { app: DAILY_APP, channel: DAILY_CHANNEL });
  for (const value of [{ token: 'x\r\ninjected', expiresAtUnixMs: 1234 }, { token: 'x', expiresAtUnixMs: '1234' }, null]) {
    await assert.rejects(mintAppUpdateToken('https://gateway.example', 'paired-secret', async () => new Response(JSON.stringify({ value }))));
  }
  await assert.rejects(mintAppUpdateToken('https://gateway.example', 'paired-secret', async () => new Response('', { status: 403 })));
});

test('fixed gateway root routes refuse reverse-proxy prefixes before minting', async () => {
  const f = fixture();
  f.session.setPairing('https://gateway.example/prefix', 'paired-secret');
  await f.session.check();
  assert.equal(f.events.some(event => event[0] === 'mint'), false);
  assert.equal(f.events.some(event => event[0] === 'gateway'), false);
  f.session.close();
});
