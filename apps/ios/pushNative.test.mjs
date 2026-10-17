import assert from 'node:assert/strict';
import { test } from 'node:test';
import { nativePushPorts, pushWrite, PUSH_REQUEST_TIMEOUT_MS } from './pushNative.ts';

const storage = { readEnabled: async () => false, writeEnabled: async () => {} };

test('native adapter uses provisioned APNs environment rather than release type', async () => {
  const ports = nativePushPorts(storage);
  globalThis.testPushExpoConfig = { extra: { stIosPush: true } };
  for (const environment of ['development', 'production', null]) {
    globalThis.testPushApplicationNative = { environment: async () => environment };
    assert.equal(await ports.environment(), environment ?? undefined);
  }
  globalThis.testPushApplicationNative = { environment: async () => { throw new Error('native unavailable'); } };
  assert.equal(await ports.environment(), undefined);
  assert.equal(ports.bundleId, 'com.example.smalltalk'); assert.equal(ports.version, '1.0');
});

test('ordinary builds cannot capture APNs even with an APNs-capable provisioning profile', async () => {
  let inspected = false;
  globalThis.testPushApplicationNative = { environment: async () => { inspected = true; return 'development'; } };
  for (const config of [undefined, {}, { extra: {} }, { extra: { stIosPush: false } }]) {
    globalThis.testPushExpoConfig = config;
    assert.equal(await nativePushPorts(storage).environment(), undefined);
  }
  assert.equal(inspected, false);
});

test('native permission reads do not prompt, intentional requests use alert permission', async () => {
  const calls = [];
  let settings = { granted: false, ios: { status: 3 } };
  globalThis.testPushNotificationsNative = { permission: async (...args) => { calls.push(args); return settings; } };
  const ports = nativePushPorts(storage);
  assert.equal(await ports.permission(false), true, 'provisional permission authorizes registration');
  assert.equal(await ports.permission(true), true);
  assert.deepEqual(calls, [[false], [true, { ios: { allowAlert: true, allowBadge: true, allowSound: true } }]]);
  settings = { granted: false, ios: { status: 1 } }; assert.equal(await ports.permission(false), false);
  settings = { granted: true, ios: { status: 2 } }; assert.equal(await ports.permission(false), true);
});

test('native adapter captures direct iOS tokens and releases its change subscription', async () => {
  let nativeToken = { type: 'ios', data: 'ab'.repeat(32) }, changed, removed = false;
  globalThis.testPushNotificationsNative = {
    token: async () => nativeToken,
    listen: callback => { changed = callback; return { remove: () => { removed = true; } }; },
  };
  const ports = nativePushPorts(storage);
  assert.equal(await ports.token(), nativeToken.data);
  nativeToken = { type: 'android', data: 'not-an-apns-token' }; assert.equal(await ports.token(), undefined);
  const tokens = [];
  const stop = ports.listen(token => tokens.push(token));
  changed({ type: 'android', data: 'not-an-apns-token' }); changed({ type: 'ios', data: 'cd'.repeat(32) });
  assert.deepEqual(tokens, ['cd'.repeat(32)]); stop(); assert.equal(removed, true);
});

test('push-only request deadline aborts a stalled transport without exposing request bytes', async t => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  let signal;
  const pending = pushWrite(async abortSignal => {
    signal = abortSignal;
    return new Promise(() => {});
  });
  const rejected = assert.rejects(pending, error => /timed out/.test(error.message) && !error.message.includes('private-token-sentinel'));
  t.mock.timers.tick(PUSH_REQUEST_TIMEOUT_MS);
  await rejected;
  assert.equal(signal.aborted, true);
});

test('completed push requests clear their deadline', async t => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  let signal;
  const response = {};
  assert.equal(await pushWrite(async abortSignal => { signal = abortSignal; return response; }), response);
  t.mock.timers.tick(PUSH_REQUEST_TIMEOUT_MS);
  assert.equal(signal.aborted, false);
});

test('push deadline includes response body decoding after the transport returns headers', async t => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  let signal, bodyStarted, finishBody;
  const started = new Promise(resolve => { bodyStarted = resolve; });
  const pending = pushWrite(async abortSignal => {
    signal = abortSignal;
    const response = new Response(new ReadableStream({ start(controller) { bodyStarted(); finishBody = () => controller.close(); } }));
    return response.json();
  });
  const rejected = assert.rejects(pending, /timed out/);
  await started;
  t.mock.timers.tick(PUSH_REQUEST_TIMEOUT_MS);
  await rejected;
  finishBody();
  assert.equal(signal.aborted, true);
});
