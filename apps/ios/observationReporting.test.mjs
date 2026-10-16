import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import ts from 'typescript';
import { MINUTE_MS, Observations, toObservationReport } from './observations.ts';

// Execute the production callback without importing React Native's StoreProvider into Node.
// The actual Observations instance synchronously calls ready while nextReport sweeps a minute.
const source = ts.createSourceFile('store.tsx', readFileSync(new URL('./store.tsx', import.meta.url), 'utf8'), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
const callbacks = [];
const visit = node => {
  if (ts.isBinaryExpression(node) && node.operatorToken.kind === ts.SyntaxKind.EqualsToken && node.left.getText(source) === 'state.send' && ts.isArrowFunction(node.right)) callbacks.push(node.right);
  ts.forEachChild(node, visit);
};
visit(source);
assert.equal(callbacks.length, 1);
const callback = ts.transpile(`return (${callbacks[0].getText(source)});`, { target: ts.ScriptTarget.ES2022 });
// A stand-in with the client's shape: the callback only asks `instanceof` and `status`.
class ClientError extends Error { constructor(status) { super(`status ${status}`); this.status = status; } }
const bindSend = new Function('state', 'observations', 'toObservationReport', 'client', 'ClientError', callback);
const setup = (stored = null) => {
  const clock = { at: Date.UTC(2026, 9, 14, 12, 0, 5), now() { return this.at; }, id: () => 'report/demo' };
  const observations = new Observations({ read: async () => stored, write: async () => {} }, clock, () => ({ carrier: 'tailscale' }));
  const state = { busy: false, last: 0, send: undefined };
  const sent = [];
  const pending = Promise.withResolvers();
  const client = { observationsReport: report => { sent.push(report); return pending.promise; } };
  state.send = bindSend(state, observations, toObservationReport, client, ClientError);
  observations.ready = state.send;
  return { clock, observations, state, sent, pending };
};

test('report preparation reserves the send slot before a minute sweep reenters ready', async () => {
  const { clock, observations, state, sent, pending } = setup();
  observations.observe('ios-connect', 100);
  clock.at += MINUTE_MS;
  assert.doesNotThrow(() => state.send(), 'nextReport → sweep → ready must not recursively prepare another report');
  assert.equal(sent.length, 1);
  assert.equal(sent[0].samples.length, 1);
  assert.equal(state.busy, true);
  state.send();
  assert.equal(sent.length, 1, 'one report is in flight');
  pending.resolve({});
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(state.busy, false);
  assert.equal(observations.waiting, 0);
});

test('a null report releases the preparation slot without throttling the next measurement', () => {
  const { state, sent } = setup();
  state.send();
  assert.equal(state.busy, false);
  assert.equal(state.last, 0);
  assert.equal(sent.length, 0);
});

test('an empty restored batch is acknowledged and releases the preparation slot', async () => {
  const start = Date.UTC(2026, 9, 14, 11, 59);
  const { observations, state, sent } = setup(JSON.stringify({ closed: [{ start, series: {}, shares: {} }] }));
  observations.ready = undefined;
  await observations.load();
  observations.ready = state.send;
  state.send();
  assert.equal(state.busy, false);
  assert.equal(state.last, 0);
  assert.equal(observations.waiting, 0);
  assert.equal(sent.length, 0);
});

test('a synchronous preparation failure releases busy and the next send still runs', async () => {
  const { clock, observations, state, sent, pending } = setup();
  const expected = new Error('invented preparation failure');
  const nextReport = observations.nextReport.bind(observations);
  observations.nextReport = () => { throw expected; };
  assert.throws(() => state.send(), error => error === expected);
  assert.equal(state.busy, false);
  assert.equal(state.last, 0);
  observations.nextReport = nextReport;
  observations.observe('ios-connect', 100);
  clock.at += MINUTE_MS;
  state.send();
  assert.equal(sent.length, 1);
  pending.resolve({});
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(state.busy, false);
});

test('transport rejection releases busy and preserves the batch for the next send', async () => {
  const { clock, observations, state, sent, pending } = setup();
  observations.observe('ios-connect', 100);
  clock.at += MINUTE_MS;
  state.send();
  assert.equal(state.busy, true);
  pending.reject(new Error('invented transport failure'));
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(state.busy, false);
  assert.equal(observations.waiting, 1);
  state.last = 0;
  state.send();
  assert.equal(sent.length, 2);
  assert.deepEqual(sent[1], sent[0]);
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(state.busy, false);
});

test('a report the member refuses for its content is dropped, any other failure keeps it', async () => {
  for (const [status, kept] of [[400, 0], [413, 0], [422, 0], [404, 1], [429, 1], [503, 1]]) {
    const { clock, observations, state, pending } = setup();
    observations.observe('ios-connect', 100);
    clock.at += MINUTE_MS;
    state.send();
    pending.reject(new ClientError(status));
    await Promise.resolve();
    await Promise.resolve();
    assert.equal(state.busy, false, `${status}`);
    assert.equal(observations.waiting, kept, `${status}: a ${kept ? 'retryable' : 'refused'} report`);
  }
});
