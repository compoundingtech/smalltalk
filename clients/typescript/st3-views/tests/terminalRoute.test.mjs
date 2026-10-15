import assert from 'node:assert/strict';
import { test } from 'node:test';
import { terminalRoute, TerminalRouteMemory, fabricRouteLabel, daemonRouteLabel } from '../terminalRoute.ts';
const route = { terminal_id: 'terminal/agent/demo/worker', runtime_incarnation: 'runtime:one', owner_host_id: 'host/owner-a', fabric: { node: 'a'.repeat(64), service: 'st3/client/demo-fleet' } };
test('route fences subject/incarnation and admits only the client service', () => {
  assert.equal(terminalRoute(route, route.terminal_id, 'runtime:one'), route);
  for (const change of [{ terminal_id: 'terminal/other' }, { terminal_id: undefined }, { runtime_incarnation: true }, { owner_host_id: 42 }, { fabric: { ...route.fabric, node: ['a'.repeat(64)] } }, { fabric: { ...route.fabric, service: 42 } }, { runtime_incarnation: '' }, { owner_host_id: '' }, { fabric: { ...route.fabric, service: 'st3/pty/demo-fleet' } }, { fabric: { ...route.fabric, node: 'arbitrary' } }]) assert.throws(() => terminalRoute({ ...route, ...change }, route.terminal_id));
  assert.throws(() => terminalRoute({ ...route, runtime_incarnation: 'runtime:two' }, route.terminal_id, 'runtime:one'), /restarted/);
  assert.equal(fabricRouteLabel(route), 'Fabric to owner-a');
  assert.equal(daemonRouteLabel('owner away', true), 'via the daemon (Fabric: owner away, not tried again yet)');
});
test('memory expires from the real attempt and is scoped to owner/node/service', () => {
  let now = 0; const memory = new TerminalRouteMemory(() => now);
  memory.failed(route, 'owner away'); now = 59_999;
  assert.equal(memory.reason(route), 'owner away');
  assert.equal(memory.reason({ ...route, fabric: { ...route.fabric, node: 'b'.repeat(64) } }), undefined);
  assert.equal(memory.reason({ ...route, fabric: { ...route.fabric, service: 'st3/client/another-fleet' } }), undefined);
  now = 60_000; assert.equal(memory.reason(route), undefined, 'skipped reads do not extend the window');
  memory.failed(route, 'new failure'); memory.succeeded(route); assert.equal(memory.reason(route), undefined);
});
