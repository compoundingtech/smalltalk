import assert from 'node:assert/strict';
import { test } from 'node:test';
import { readFileSync } from 'node:fs';
import ts from 'typescript';
import { PushRegistrationLifecycle, pushDecision, pushRequest } from './pushRegistration.ts';

const token = 'ab'.repeat(32);
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
function harness({ enabled = false, environment = 'development', permission = true, owners = enabled ? ['A'] : [] } = {}) {
  const calls = [];
  const state = { saved: enabled, owners: [...owners], permission, token, changed: undefined, stops: 0 };
  const ports = {
    bundleId: 'com.example.smalltalk', version: '1.0',
    readEnabled: async () => state.saved,
    writeEnabled: async value => { calls.push(['setting', value]); state.saved = value; },
    environment: async () => environment,
    permission: async request => { calls.push(['permission', request]); return state.permission; },
    token: async () => { calls.push(['token']); return state.token; },
    listen: changed => { state.changed = changed; return () => { state.stops++; state.changed = undefined; }; },
  };
  const remote = {
    register: async request => { calls.push(['register', request]); return { registered: true, environment: request.environment }; },
    unregister: async () => { calls.push(['unregister']); return { registered: false }; },
  };
  const ownership = {
    read: async () => [...state.owners],
    write: async issuers => { state.owners = [...issuers]; },
  };
  const lifecycle = new PushRegistrationLifecycle(ports, ownership);
  return { calls, state, ports, ownership, remote, lifecycle, connect: () => lifecycle.configure({ paired: true, granted: true, issuer: 'A', issuers: new Map([['A', remote]]), remote }) };
}

test('decision matrix gates all native acquisition and paired writes', () => {
  for (const paired of [false, true]) for (const granted of [false, true]) for (const enabled of [false, true]) for (const available of [false, true]) for (const permission of [false, true]) {
    const expected = !paired || !granted ? 'none' : !enabled || !permission ? 'unregister' : available ? 'register' : 'none';
    assert.equal(pushDecision({ paired, granted, enabled, available, permission }), expected);
  }
});

test('request uses only bounded native metadata and never reflects malformed tokens', () => {
  assert.deepEqual(pushRequest(token, 'development', 'com.example.smalltalk', '1.0'), { token, environment: 'development', bundle_id: 'com.example.smalltalk', app_version: '1.0' });
  for (const value of ['', 'a', 'abc', 'zz', 'ab'.repeat(257), { token }, 'secret-sentinel']) assert.equal(pushRequest(value, 'production', 'com.example.smalltalk', '1.0'), undefined);
  assert.ok(pushRequest('AB', 'production', 'a', 'v'));
  assert.ok(pushRequest('ab'.repeat(256), 'production', 'a'.repeat(255), 'v'.repeat(64)));
  for (const bundle of [null, '', 'a'.repeat(256), 'com.example/invalid', 'non-ascii-ä']) assert.equal(pushRequest(token, 'development', bundle, '1.0'), undefined);
  for (const version of [null, '', 'v'.repeat(65), 'newline\n', 'non-ascii-ä']) assert.equal(pushRequest(token, 'development', 'com.example.smalltalk', version), undefined);
});

test('default-off launch without cleanup owners never contacts a remote or requests native permission', async () => {
  const h = harness(); h.connect(); await h.lifecycle.start();
  assert.equal(h.lifecycle.snapshot().enabled, false);
  assert.deepEqual(h.calls, []);
  assert.equal(h.state.changed, undefined);
  await h.lifecycle.setEnabled(true);
  assert.equal(h.state.saved, true);
  assert.equal(h.lifecycle.snapshot().status, 'registered');
  assert.deepEqual(h.calls.map(call => call[0]), ['setting', 'permission', 'token', 'register']);
  assert.deepEqual(h.calls.find(call => call[0] === 'permission'), ['permission', true]);
});

test('launch and token rotation register directly with APNs without a permission prompt', async () => {
  const h = harness({ enabled: true, environment: 'production' }); h.connect(); await h.lifecycle.start();
  assert.equal(h.lifecycle.snapshot().environment, 'production');
  assert.equal(h.lifecycle.snapshot().status, 'registered');
  h.calls.length = 0;
  h.state.changed('cd'.repeat(32));
  await h.lifecycle.refresh();
  assert.equal(h.calls.find(call => call[0] === 'register')[1].token, 'cd'.repeat(32));
  assert.ok(h.calls.filter(call => call[0] === 'permission').every(call => call[1] === false));
  assert.equal(JSON.stringify(h.lifecycle.snapshot()).includes(token), false);
  assert.equal(JSON.stringify(h.lifecycle.snapshot()).includes('cd'.repeat(32)), false);
});

test('unpaired or unsupported sessions never read permission, acquire tokens or mutate', async () => {
  for (const context of [{ paired: false, granted: true }, { paired: true, granted: false }]) {
    const h = harness({ enabled: true });
    h.lifecycle.configure({ ...context, issuer: 'A', issuers: new Map(), remote: h.remote });
    await h.lifecycle.start(); await h.lifecycle.setEnabled(true);
    assert.deepEqual(h.calls, []); assert.equal(h.state.changed, undefined);
  }
});

test('a missing entitlement or unknown environment never requests permission or acquires a token', async () => {
  const h = harness({ environment: null }); h.connect(); await h.lifecycle.start(); h.calls.length = 0;
  await h.lifecycle.setEnabled(true);
  assert.equal(h.lifecycle.snapshot().enabled, false); assert.deepEqual(h.calls, []);
});

test('denied permission removes prior registration; launch still does not prompt', async () => {
  const h = harness({ enabled: true, permission: false }); h.connect(); await h.lifecycle.start();
  assert.equal(h.lifecycle.snapshot().status, 'permission-denied');
  assert.deepEqual(h.calls, [['permission', false], ['unregister']]);
  await h.lifecycle.setEnabled(false);
  assert.equal(h.state.saved, false); assert.equal(h.state.changed, undefined);
});

test('an opt-out while native acquisition is pending fences the token before any register', async () => {
  const h = harness(); h.connect(); await h.lifecycle.start(); h.calls.length = 0;
  const acquired = deferred(), reading = deferred();
  h.ports.token = () => { reading.resolve(); return acquired.promise; };
  const enabling = h.lifecycle.setEnabled(true); await reading.promise;
  const disabling = h.lifecycle.setEnabled(false); acquired.resolve(token);
  await Promise.all([enabling, disabling]);
  assert.equal(h.calls.some(call => call[0] === 'register'), false);
  assert.equal(h.calls.at(-1)[0], 'unregister'); assert.equal(h.state.saved, false);
});

test('opt-out serializes unregister after an in-flight register', async () => {
  const h = harness(); h.connect(); await h.lifecycle.start(); h.calls.length = 0;
  const sent = deferred(), reply = deferred();
  h.remote.register = async request => { h.calls.push(['register', request]); sent.resolve(); return reply.promise; };
  const enabling = h.lifecycle.setEnabled(true); await sent.promise;
  const disabling = h.lifecycle.setEnabled(false); reply.resolve({ registered: true, environment: 'development' });
  await Promise.all([enabling, disabling]);
  assert.equal(h.calls.at(-1)[0], 'unregister'); assert.equal(h.lifecycle.snapshot().status, 'unregistered');
});

test('pairing/capability loss and teardown fence late tokens and remove native listeners', async () => {
  for (const end of ['stop', 'capability', 'pairing']) {
    const h = harness(); h.connect(); await h.lifecycle.start();
    const reading = deferred(), acquired = deferred();
    h.ports.token = () => { reading.resolve(); return acquired.promise; };
    const enabling = h.lifecycle.setEnabled(true); await reading.promise;
    const changed = h.state.changed;
    if (end === 'stop') h.lifecycle.stop();
    else h.lifecycle.configure({ paired: end !== 'pairing', granted: false, issuer: 'A', issuers: new Map(), remote: h.remote });
    acquired.resolve(token); changed?.('cd'.repeat(32)); await enabling; await h.lifecycle.refresh();
    assert.equal(h.calls.some(call => call[0] === 'register'), false);
    assert.equal(h.state.changed, undefined); assert.ok(h.state.stops > 0);
  }
});

test('offline opt-out persists and unregisters at the next capable connection', async () => {
  const h = harness({ enabled: true }); h.connect(); await h.lifecycle.start(); h.calls.length = 0;
  h.lifecycle.configure({ paired: true, granted: false, issuers: new Map([['A', undefined]]) }); await h.lifecycle.setEnabled(false);
  assert.equal(h.state.saved, false); assert.equal(h.calls.some(call => call[0] === 'unregister'), false);
  assert.equal(h.lifecycle.snapshot().cleanup, 'pending');
  assert.notEqual(h.lifecycle.snapshot().status, 'unregistered');
  h.connect(); await h.lifecycle.refresh();
  assert.equal(h.calls.at(-1)[0], 'unregister'); assert.equal(h.state.changed, undefined);
});

test('failed Forget retains cleanup ownership and exposes only safe retry state', async () => {
  const h = harness({ enabled: true }); h.connect(); await h.lifecycle.start();
  const changed = h.state.changed;
  h.calls.length = 0;
  h.remote.unregister = async () => { h.calls.push(['unregister']); throw new Error(token); };
  assert.equal(await h.lifecycle.forget(), false); changed(token); await h.lifecycle.refresh();
  assert.deepEqual(h.calls.slice(0, 2), [['setting', false], ['unregister']]);
  assert.equal(h.lifecycle.snapshot().cleanup, 'failed');
  assert.equal(h.lifecycle.snapshot().status, 'failed');
  assert.deepEqual(h.state.owners, ['A']);
  assert.equal(h.state.saved, false); assert.equal(h.state.changed, undefined);
  assert.equal(JSON.stringify(h.lifecycle.snapshot()).includes(token), false);
});

test('forget waits for an in-flight registration before unregistering', async () => {
  const h = harness(); h.connect(); await h.lifecycle.start(); h.calls.length = 0;
  const sent = deferred(), reply = deferred();
  h.remote.register = async request => { h.calls.push(['register', request]); sent.resolve(); return reply.promise; };
  const enabling = h.lifecycle.setEnabled(true); await sent.promise;
  const forgetting = h.lifecycle.forget(); reply.resolve({ registered: true, environment: 'development' });
  await Promise.all([enabling, forgetting]);
  assert.equal(h.calls.at(-1)[0], 'unregister'); assert.equal(h.state.saved, false);
});

test('network/native failures expose only safe status, never token-bearing exception text', async () => {
  for (const failure of ['permission', 'token', 'register']) {
    const h = harness({ enabled: true });
    if (failure === 'register') h.remote.register = async () => { throw new Error(token); };
    else h.ports[failure] = async () => { throw new Error(token); };
    h.connect(); await h.lifecycle.start();
    assert.equal(h.lifecycle.snapshot().status, 'failed');
    assert.equal(JSON.stringify(h.lifecycle.snapshot()).includes(token), false);
  }
});

test('opt-out and forget do not wait for native permission/token callbacks that never arrive', async () => {
  for (const operation of ['off', 'forget']) for (const pending of ['permission', 'token']) {
    const h = harness(); h.connect(); await h.lifecycle.start(); h.calls.length = 0;
    const entered = deferred(), native = deferred();
    h.ports[pending] = () => { entered.resolve(); return native.promise; };
    const enabling = h.lifecycle.setEnabled(true); await entered.promise;
    if (operation === 'off') await h.lifecycle.setEnabled(false); else await h.lifecycle.forget();
    await enabling;
    assert.equal(h.calls.some(call => call[0] === 'register'), false);
    assert.equal(h.calls.filter(call => call[0] === 'unregister').length, pending === 'token' ? 1 : 0);
    assert.equal(h.lifecycle.snapshot().cleanup, 'complete');
    assert.equal(h.state.saved, false);
    native.resolve(pending === 'permission' ? true : token);
    await h.lifecycle.refresh();
    assert.equal(h.calls.some(call => call[0] === 'register'), false);
  }
});

test('distinct issuers A then B are both cleaned by disable and Forget', async () => {
  for (const operation of ['disable', 'forget']) {
    const h = harness({ enabled: true }); h.connect(); await h.lifecycle.start();
    const cleaned = [];
    h.remote.unregister = async () => { cleaned.push('A'); return { registered: false }; };
    const b = {
      register: async () => ({ registered: true }),
      unregister: async () => { cleaned.push('B'); return { registered: false }; },
    };
    h.lifecycle.configure({ paired: true, granted: true, issuer: 'B', remote: b, issuers: new Map([['A', h.remote], ['B', b]]) });
    await h.lifecycle.refresh();
    assert.deepEqual(h.state.owners, ['A', 'B']);
    if (operation === 'disable') await h.lifecycle.setEnabled(false);
    else assert.equal(await h.lifecycle.forget(), true);
    assert.deepEqual(cleaned, ['A', 'B']);
    assert.deepEqual(h.state.owners, []);
    assert.equal(h.lifecycle.snapshot().cleanup, 'complete');
  }
});

test('a fresh phone lifecycle recovers all saved issuer ownership without retaining route wrappers', async () => {
  const h = harness({ owners: ['A', 'B'] });
  const cleaned = [];
  const route = name => ({ register: async () => ({ registered: true }), unregister: async () => { cleaned.push(name); return { registered: false }; } });
  h.lifecycle.configure({ paired: true, granted: true, issuer: 'B', remote: route('old-B'), issuers: new Map([['A', undefined], ['B', undefined]]) });
  await h.lifecycle.start();
  assert.equal(h.lifecycle.snapshot().cleanup, 'pending');
  assert.deepEqual(cleaned, [], 'an unavailable issuer must not use a stale route wrapper');
  for (const name of ['A', 'B']) {
    const remote = route(name);
    h.lifecycle.configure({ paired: true, granted: true, issuer: name, remote, issuers: new Map([[name, remote]]) });
    await h.lifecycle.refresh();
  }
  assert.deepEqual(cleaned, ['A', 'B']);
  assert.deepEqual(h.state.owners, []);
  assert.equal(await h.lifecycle.forget(), true);
});

test('never-enabled phones can Forget or remove an unowned profile offline without remote contact', async () => {
  const h = harness();
  h.lifecycle.configure({ paired: true, granted: false, issuers: new Map([['A', undefined]]) });
  await h.lifecycle.start();
  assert.equal(await h.lifecycle.forgetIssuer('A'), true);
  assert.equal(await h.lifecycle.forget(), true);
  assert.deepEqual(h.calls, [['setting', false]]);
});

test('journal persistence precedes token acquisition and register, including lost-response recovery', async () => {
  const h = harness({ enabled: true, owners: [] });
  h.ports.token = async () => { assert.deepEqual(h.state.owners, ['A']); return token; };
  h.remote.register = async () => { assert.deepEqual(h.state.owners, ['A']); throw new Error(token); };
  h.connect(); await h.lifecycle.start();
  assert.equal(h.lifecycle.snapshot().status, 'failed');
  assert.deepEqual(h.state.owners, ['A']);
  h.remote.unregister = async () => ({ registered: false });
  assert.equal(await h.lifecycle.forget(), true);
  assert.deepEqual(h.state.owners, []);
});

test('unreadable ownership and unwritable registration journal fail closed without tokens or credential deletion', async () => {
  for (const failure of ['read', 'write']) {
    const h = harness({ enabled: true, owners: [] });
    h.ownership[failure] = async () => { throw new Error(token); };
    h.connect(); await h.lifecycle.start();
    assert.equal(h.calls.some(call => call[0] === 'token' || call[0] === 'register'), false);
    assert.equal(h.lifecycle.snapshot().status, 'failed');
    if (failure === 'read') {
      assert.equal(await h.lifecycle.forget(), false);
      assert.equal(await h.lifecycle.forgetIssuer('A'), false);
    }
    assert.equal(JSON.stringify(h.lifecycle.snapshot()).includes(token), false);
  }
});

test('confirmed unregister cannot release ownership until its journal removal is durable', async () => {
  const h = harness({ enabled: true }); h.connect(); await h.lifecycle.start();
  h.ownership.write = async () => { throw new Error(token); };
  assert.equal(await h.lifecycle.forget(), false);
  assert.deepEqual(h.state.owners, ['A']);
  assert.equal(h.lifecycle.snapshot().cleanup, 'failed');
  assert.equal(h.lifecycle.snapshot().status, 'failed');
  h.ownership.write = async owners => { h.state.owners = [...owners]; };
  assert.equal(await h.lifecycle.forget(), true);
});

test('permission denial removes every owned issuer and retains failed ones for reconnect retry', async () => {
  const h = harness({ enabled: true, permission: false, owners: ['A', 'B'] });
  h.connect(); await h.lifecycle.start();
  assert.deepEqual(h.state.owners, ['B']);
  assert.equal(h.lifecycle.snapshot().cleanup, 'pending');
  assert.notEqual(h.lifecycle.snapshot().status, 'permission-denied');
  const b = { register: async () => ({ registered: true }), unregister: async () => ({ registered: false }) };
  h.lifecycle.configure({ paired: true, granted: true, issuer: 'B', remote: b, issuers: new Map([['B', b]]) });
  await h.lifecycle.refresh();
  assert.deepEqual(h.state.owners, []);
  assert.equal(h.lifecycle.snapshot().status, 'permission-denied');
});

test('expired and revoked registrations require exact-old-grant cleanup confirmation before Forget or issuer removal', async () => {
  for (const retirement of ['expired', 'revoked']) for (const operation of ['forget', 'issuer']) {
    const h = harness({ enabled: true }); h.connect(); await h.lifecycle.start();
    assert.equal(h.lifecycle.snapshot().status, 'registered');
    h.calls.length = 0;
    const entered = deferred(), reply = deferred();
    // Model the API boundary: normal authority is gone, but this issuer still has
    // the saved old bearer and can explicitly confirm its exact grant's DELETE.
    h.remote.register = async () => { assert.fail(`${retirement} pairing must not register`); };
    h.remote.unregister = async () => { h.calls.push(['unregister']); entered.resolve(); return reply.promise; };
    h.lifecycle.configure({ paired: true, granted: false, issuer: 'A', issuers: new Map([['A', h.remote]]) });
    await h.lifecycle.refresh();
    assert.deepEqual(h.calls, []);
    assert.equal(h.state.changed, undefined);
    const cleaning = operation === 'forget' ? h.lifecycle.forget() : h.lifecycle.forgetIssuer('A');
    await entered.promise;
    assert.deepEqual(h.state.owners, ['A'], 'server-side expiry/revocation alone is not phone cleanup confirmation');
    reply.resolve({ registered: false });
    assert.equal(await cleaning, true);
    assert.deepEqual(h.calls.filter(call => call[0] === 'unregister'), [['unregister']]);
    assert.deepEqual(h.state.owners, []);
    assert.equal(h.lifecycle.snapshot().cleanup, 'complete');
    assert.equal(await h.lifecycle.forget(), true, 'confirmed cleanup permits subsequent Forget without another DELETE');
    assert.equal(h.calls.filter(call => call[0] === 'unregister').length, 1);
  }
});

test('re-pair after expiry or revocation cleans the old incarnation before registering the replacement', async () => {
  for (const retirement of ['expired', 'revoked']) {
    const h = harness({ enabled: true }); h.connect(); await h.lifecycle.start();
    const calls = [];
    h.remote.register = async () => { assert.fail(`${retirement} pairing must not register`); };
    h.remote.unregister = async () => { calls.push('old-delete'); return { registered: false }; };
    h.lifecycle.configure({ paired: true, granted: false, issuer: 'A', issuers: new Map([['A', h.remote]]) });
    await h.lifecycle.refresh();
    // completePairing uses this gate before replacing the saved profile, then
    // releases the retiring profile key in its finally block.
    assert.equal(await h.lifecycle.forgetIssuer('A'), true);
    assert.deepEqual(h.state.owners, []);
    const replacement = {
      register: async () => { calls.push('new-register'); return { registered: true }; },
      unregister: async () => { calls.push('new-delete'); return { registered: false }; },
    };
    h.lifecycle.releaseIssuer('A');
    h.lifecycle.configure({ paired: true, granted: true, issuer: 'A', remote: replacement, issuers: new Map([['A', replacement]]) });
    await h.lifecycle.refresh();
    assert.equal(calls[0], 'old-delete');
    assert.ok(calls.slice(1).every(call => call === 'new-register'));
    assert.ok(calls.includes('new-register'));
    assert.deepEqual(h.state.owners, ['A']);
    assert.equal(h.lifecycle.snapshot().status, 'registered');
    assert.equal(await h.lifecycle.forget(), true);
    assert.equal(calls.at(-1), 'new-delete');
    assert.equal(calls.filter(call => call === 'old-delete').length, 1);
    assert.deepEqual(h.state.owners, []);
  }
});

test('retired pairing cleanup still blocks Forget, removal and re-pair on transport, authorization or journal failures', async () => {
  for (const retirement of ['expired', 'revoked']) for (const operation of ['forget', 'issuer', 're-pair']) for (const failure of ['transport', '401', '403', 'journal']) {
    const h = harness({ enabled: true }); h.connect(); await h.lifecycle.start();
    h.remote.register = async () => { assert.fail(`${retirement} pairing must not register`); };
    h.remote.unregister = async () => {
      if (failure === 'journal') return { registered: false };
      throw Object.assign(new Error(token), failure === 'transport' ? {} : { status: Number(failure) });
    };
    if (failure === 'journal') h.ownership.write = async () => { throw new Error(token); };
    h.lifecycle.configure({ paired: true, granted: false, issuer: 'A', issuers: new Map([['A', h.remote]]) });
    await h.lifecycle.refresh();
    const clean = () => operation === 'forget' ? h.lifecycle.forget() : h.lifecycle.forgetIssuer('A');
    assert.equal(await clean(), false, `${retirement}/${operation}/${failure} must retain the old profile`);
    assert.deepEqual(h.state.owners, ['A']);
    assert.equal(h.lifecycle.snapshot().cleanup, 'failed');
    assert.equal(JSON.stringify(h.lifecycle.snapshot()).includes(token), false);
    h.remote.unregister = async () => ({ registered: false });
    h.ownership.write = async owners => { h.state.owners = [...owners]; };
    assert.equal(await clean(), true, 'only explicit confirmation and durable journal removal release the gate');
    assert.deepEqual(h.state.owners, []);
  }
});

// Exercise the real profile cleanup helper without importing React Native into Node,
// matching observationReporting.test.mjs's production-callback extraction pattern.
const storeSource = ts.createSourceFile('store.tsx', readFileSync(new URL('./store.tsx', import.meta.url), 'utf8'), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
const profileHelpers = [];
const pushRouteDeclarations = new Map();
const visitStore = node => {
  if (ts.isFunctionDeclaration(node) && node.name?.text === 'forgetMemberProfile') profileHelpers.push(node);
  if (ts.isVariableDeclaration(node) && ts.isIdentifier(node.name) && ['pushIssuers', 'pushRemote'].includes(node.name.text)) {
    assert.equal(pushRouteDeclarations.has(node.name.text), false);
    pushRouteDeclarations.set(node.name.text, node);
  }
  ts.forEachChild(node, visitStore);
};
visitStore(storeSource);
assert.equal(profileHelpers.length, 1);
const bindProfileCleanup = new Function('memberProfileKey', 'ownProfiles', 'push', 'SecureStore', 'removeDeviceKey', 'setOwnProfiles', 'setError',
  ts.transpile(`return (${profileHelpers[0].getText(storeSource)});`, { target: ts.ScriptTarget.ES2022 }));
assert.equal(pushRouteDeclarations.size, 2);
const bindPushRoutes = new Function('context', ts.transpile(`
  const { useMemo, proof, credential, PROFILE_KEY, url, isFabricGatewayId, ownProfiles,
    connectUrl, clientCredential, pushIssuer, pushGranted, status, St3Client, pushWrite,
    gatewayFetch, clientName, app } = context;
  const ${pushRouteDeclarations.get('pushIssuers').getText(storeSource)};
  const ${pushRouteDeclarations.get('pushRemote').getText(storeSource)};
  return { issuers: pushIssuers, remote: pushRemote };
`, { target: ts.ScriptTarget.ES2022 }));

test('real Fabric issuer routes allow retired-bearer cleanup without restoring registration authority', async () => {
  for (const retirement of ['expired', 'revoked']) for (const [status, pushGranted] of [['offline', false], ['offline', true], ['online', false], ['online', true]]) {
    const calls = [], bearer = `${retirement}-old-bearer`;
    const context = {
      useMemo: callback => callback(), proof: false, credential: bearer, PROFILE_KEY: 'A',
      url: 'fabric:A', isFabricGatewayId: value => value.startsWith('fabric:'),
      ownProfiles: new Map([['B', { url: 'fabric:B', credential: 'other-bearer' }]]),
      connectUrl: 'http://127.0.0.1:12345', clientCredential: bearer, pushIssuer: 'A', pushGranted, status,
      St3Client: class {
        constructor(options) { this.options = options; }
        async pushUnregister() {
          calls.push([this.options.baseUrl, this.options.credential()]);
          return { value: { registered: false } };
        }
      },
      pushWrite: callback => callback(new AbortController().signal),
      gatewayFetch: fetchImpl => fetchImpl, clientName: () => 'test', app: { expo: { version: '1.0' } },
    };
    const routes = bindPushRoutes(context);
    assert.ok(routes.issuers.get('A'), 'current member-bound carrier must survive capability/session loss');
    assert.equal(routes.issuers.get('B'), undefined, 'another member must not inherit the current carrier or bearer');
    assert.equal(routes.remote, status === 'online' && pushGranted ? routes.issuers.get('A') : undefined,
      'ordinary registration still needs both a live session and a granted capability');
    assert.deepEqual(await routes.issuers.get('A').unregister(), { registered: false });
    assert.deepEqual(calls, [[context.connectUrl, bearer]]);
    assert.equal(bindPushRoutes({ ...context, connectUrl: undefined }).issuers.get('A'), undefined);
    assert.equal(bindPushRoutes({ ...context, clientCredential: undefined }).issuers.get('A'), undefined);
    assert.equal(bindPushRoutes({ ...context, proof: true }).issuers.size, 0);
  }
});


test('real store member removal waits for issuer cleanup before deleting its credential and signing key', async () => {
  for (const retirement of ['active', 'expired', 'revoked']) for (const succeeds of [true, false]) {
    const h = harness({ enabled: true }); h.connect(); await h.lifecycle.start();
    if (retirement !== 'active') {
      h.remote.register = async () => { assert.fail(`${retirement} pairing must not register`); };
      h.lifecycle.configure({ paired: true, granted: false, issuer: 'A', issuers: new Map([['A', h.remote]]) });
      await h.lifecycle.refresh();
    }
    const entered = deferred(), reply = deferred(), deleted = [], issues = [];
    h.remote.unregister = async () => { entered.resolve(); return reply.promise; };
    const cleanup = bindProfileCleanup(() => 'A', new Map([['A', { signing: { handle: 'key-A' } }]]), h.lifecycle,
      { deleteItemAsync: async key => { deleted.push(key); } },
      async key => { deleted.push(key); }, () => {}, message => { issues.push(message); });
    const removing = cleanup({});
    await entered.promise;
    assert.deepEqual(deleted, []);
    if (succeeds) reply.resolve({ registered: false }); else reply.reject(new Error(token));
    assert.equal(await removing, succeeds);
    assert.deepEqual(deleted, succeeds ? ['A', 'key-A'] : []);
    if (!succeeds) {
      assert.equal(issues.length, 1);
      assert.equal(issues[0].includes(token), false);
      assert.deepEqual(h.state.owners, ['A']);
    }
  }
});
