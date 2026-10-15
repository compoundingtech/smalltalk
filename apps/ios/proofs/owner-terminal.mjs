// Invoked by the ignored Rust daemon-pair test with a private isolated descriptor.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { performance } from 'node:perf_hooks';
import WebSocket from 'ws';
import { St3Client } from '../../../clients/typescript/st3-client/index.ts';
import { TerminalRouteMemory } from '../../../clients/typescript/st3-views/terminalRoute.ts';
import { RoutedTerminalFollow } from '../terminalRoute.ts';
const fixture = JSON.parse(readFileSync(process.argv[2], 'utf8'));
assert.equal(fixture.kind, 'isolated-owner-terminal-pair');
const native = spawn(fixture.bridge, [fixture.key], { stdio: ['pipe', 'pipe', 'inherit'] });
let sequence = 0;
const calls = new Map();
createInterface({ input: native.stdout }).on('line', line => {
  const { id, result } = JSON.parse(line), pending = calls.get(id); calls.delete(id);
  if (result.ok) pending.resolve(result.value); else pending.reject(new Error(result.error));
});
const rpc = (op, args = {}) => new Promise((resolve, reject) => {
  const id = ++sequence; calls.set(id, { resolve, reject }); native.stdin.write(JSON.stringify({ id, op, ...args }) + '\n');
});
let action = 0;
const actionId = () => `action/owner-terminal-proof-${++action}`;
const socketFactory = (url, protocols, headers) => new WebSocket(url, protocols, { headers });
function client(url, mark) {
  const value = new St3Client({ baseUrl: url, credential: () => fixture.credential, client: 'isolated-owner-proof' });
  const stream = value.collectionStream.bind(value), attach = value.terminalAttach.bind(value), detach = value.terminalDetach.bind(value);
  value.collectionStream = options => stream({ ...options, socket: socketFactory,
    onFrame: frame => { if (frame.kind === 'screen') mark?.('screen', frame); options.onFrame(frame); } });
  value.terminalAttach = async parameters => { const result = await attach(parameters); mark?.('open', result); return result; };
  value.terminalDetach = parameters => {
    assert(parameters.parameters.target_id.startsWith('terminal-attachment/'));
    return detach(parameters).then(result => { mark?.('detach', result); return result; }, error => { mark?.('detach-error', error); throw error; });
  };
  return value;
}
const gateway = client(fixture.gateway);
try {
  const capabilities = (await gateway.capabilities()).value;
  assert(!capabilities.capabilities.some(item => item.id === 'terminal.input' && item.state === 'granted'));
  const route = (await gateway.terminalRoute(fixture.terminal)).value;
  assert.equal(route.owner_host_id, 'host/owner-demo');
  assert.equal(route.runtime_incarnation, fixture.incarnation);
  assert.equal(route.fabric.node, fixture.node);
  const results = [];
  let retainedTarget, releaseDone;
  for (const mode of ['direct-cold', 'direct-warm', 'gateway-cold', 'gateway-warm']) {
    const started = performance.now();
    let opened, first, follow, gatewayStream, ownerTarget, label;
    let doneResolve, doneReject, detachResolve, detachReject;
    const detached = new Promise((resolve, reject) => { detachResolve = resolve; detachReject = reject; });
    const done = new Promise((resolve, reject) => { doneResolve = resolve; doneReject = reject; });
    const mark = (event, frame) => {
      if (event === 'detach') detachResolve();
      if (event === 'detach-error') detachReject(frame);
      if (event === 'open') opened ??= performance.now() - started;
      if (event === 'screen') {
        assert.equal(frame.value.runtime_incarnation, fixture.incarnation);
        assert.equal(frame.snapshot.host_id, mode.startsWith('direct') ? 'host/owner-demo' : 'host/gateway-demo');
        assert(frame.value.lines.some(line => line.text?.includes('isolated owner ready')));
        first ??= performance.now() - started; doneResolve();
      }
    };
    const connection = client(fixture.gateway, mark);
    const routing = {
      built: () => mode.startsWith('direct'), fallback: () => true, memory: new TerminalRouteMemory(), attemptMs: 4000,
      describe: async () => (await connection.terminalRoute(fixture.terminal)).value,
      open: async descriptor => {
        const target = retainedTarget ?? await rpc('open', { target: { ...descriptor.fabric, addr: fixture.addr, mode: 'direct' } });
        retainedTarget = target;
        ownerTarget = target;
        const owner = client(target.url, mark);
        return { client: owner, cleanup: owner,
          release: after => { releaseDone = Promise.resolve(after); return releaseDone; },
          failure: async error => ({ reason: error.message, unreachable: !error.response }) };
      },
    };
    // The actual gateway collections socket remains available beside the owner socket.
    gatewayStream = await connection.collectionStream({ onFrame: frame => {
      if (frame.kind === 'screen') follow?.screen(frame.value, frame.snapshot);
      else if (frame.kind === 'error') doneReject(new Error(frame.message));
    }, onEnd: error => { if (error) doneReject(error); } });
    follow = new RoutedTerminalFollow(connection, fixture.terminal, {
      expectedIncarnation: fixture.incarnation, onScreen: screen => assert.equal(screen.runtime_incarnation, fixture.incarnation),
      onIssue: issue => { if (issue) doneReject(new Error(issue)); }, onRoute: route => { label = route; },
    }, actionId, () => gatewayStream, routing, () => true, () => {}, [1000]);
    const deadline = setTimeout(() => doneReject(new Error('isolated stream did not deliver a screen')), 15000);
    await follow.attach(); await done; clearTimeout(deadline);
    assert(opened >= 0 && first >= opened);
    assert(mode.startsWith('direct') ? label === 'Fabric to owner-demo' : label.startsWith('via the daemon ('));
    const stats = ownerTarget ? await rpc('stats', { handle: ownerTarget.handle }) : null;
    results.push({ mode, open_ms: Math.round(opened), first_screen_ms: Math.round(first), route: label,
      paths: stats ? [...new Set(stats.attempts.map(item => item.selectedPath))] : ['gateway peer relay'] });
    follow.close(); gatewayStream.close();
    await releaseDone;
    await detached;
    if (mode === 'direct-warm') {
      const owner = client(retainedTarget.url);
      const caps = (await owner.capabilities()).value;
      assert(!caps.capabilities.some(item => item.id === 'terminal.input' && item.state === 'granted'));
      const denied = await owner.terminalScreen(fixture.terminal);
      await assert.rejects(owner.terminalInput({ id: actionId(), idempotency_key: actionId(), fence: {
        snapshot_id: denied.snapshot.id, subject_revisions: {}, runtime_incarnation: fixture.incarnation,
      }, parameters: { terminal_id: fixture.terminal, mode: 'raw', value: 'must-not-arrive' } }), error => error.response?.code === 'forbidden');
      await rpc('close', { handle: retainedTarget.handle }); retainedTarget = undefined;
    }
  }
  console.log(JSON.stringify({ kind: fixture.kind, paired_scopes: capabilities.scopes ?? 'capabilities verified read-only', timings: results }));
} finally { native.stdin.end(); }
