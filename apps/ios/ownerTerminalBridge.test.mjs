import assert from 'node:assert/strict';
import { test } from 'node:test';
import { registerHooks } from 'node:module';
let opened = 0, closed = [], pendingOpen;
globalThis.ownerProofNative = {
  openTarget: async () => { const index = ++opened; if (pendingOpen) await pendingOpen; return { handle: `target-${index}`, url: `http://127.0.0.1:${40000 + index}` }; },
  closeTarget: async handle => { closed.push(handle); },
  targetStats: async () => ({ attempts: [] }),
};
registerHooks({
  resolve(specifier, context, next) { return specifier === 'expo' ? { url: 'proof:expo-native', shortCircuit: true } : next(specifier, context); },
  load(url, context, next) { return url === 'proof:expo-native' ? { format: 'module', source: 'export const requireOptionalNativeModule = () => globalThis.ownerProofNative;', shortCircuit: true } : next(url, context); },
});
const { ownerTerminalRouting } = await import('./ownerTerminalBridge.ts');
const options = { gatewayUrl: 'http://127.0.0.1:39000', credential: 'isolated-test-token', clientName: 'owner-proof', built: () => true, fallback: () => true, gatewayFabric: () => null };
const descriptor = { terminal_id: 'terminal/agent/demo/worker', runtime_incarnation: 'runtime:one', owner_host_id: 'host/owner-a', fabric: { node: 'a'.repeat(64), service: 'st3/client/demo-fleet' } };
const settle = () => new Promise(resolve => setImmediate(resolve));
test('feed replacement waits for the old issuer detach and native close', async () => {
  const old = ownerTerminalRouting(options), next = ownerTerminalRouting(options);
  const target = await old.open(descriptor, new AbortController().signal);
  let finish; const detaching = new Promise(resolve => { finish = resolve; });
  const release = target.release(detaching);
  const opening = next.open(descriptor, new AbortController().signal);
  await settle(); assert.equal(opened, 1); assert.deepEqual(closed, []);
  finish(); await release; const newTarget = await opening;
  assert.deepEqual(closed, ['target-1']); assert.equal(opened, 2); await newTarget.release();
});
test('a cancelled late open closes before the replacement opens', async () => {
  let finish; pendingOpen = new Promise(resolve => { finish = resolve; });
  const cancelled = new AbortController();
  const old = ownerTerminalRouting(options).open(descriptor, cancelled.signal);
  const rejected = assert.rejects(old, /cancelled/);
  await settle(); assert.equal(opened, 3); cancelled.abort();
  const next = ownerTerminalRouting(options).open(descriptor, new AbortController().signal);
  await settle(); assert.equal(opened, 3); pendingOpen = undefined; finish();
  await rejected; const target = await next;
  assert.deepEqual(closed, ['target-1', 'target-2', 'target-3']); assert.equal(opened, 4); await target.release();
});
