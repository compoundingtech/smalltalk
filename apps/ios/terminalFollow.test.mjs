import assert from 'node:assert/strict';
import { Feed } from './feed.ts';
import { ForegroundGate } from './foreground.ts';
import { TERMINAL_RESTARTED } from './terminalControls.ts';
import { fakeClient as collectionsClient } from './fakeFeed.mjs';

const terminalId = 'terminal/agent/worker';
const screen = (text, incarnation = 'incarnation/one') => ({ runtime_incarnation: incarnation, next_sequence: 7, lines: [{ row: 0, text }] });
const settle = (ms = 20) => new Promise(resolve => setTimeout(resolve, ms));
const noWindows = { onWindow: () => {}, onConnection: () => {} };

function fakeClient({ incarnation = 'incarnation/one', attachError } = {}) {
  const { client, sockets } = collectionsClient();
  const calls = { reads: 0, attaches: [], detaches: [], incarnation };
  client.terminalScreen = async id => { assert.equal(id, terminalId); calls.reads++; return { snapshot: { id: `snapshot/${calls.reads}` }, value: screen('current', calls.incarnation) }; };
  client.terminalAttach = async input => {
    calls.attaches.push(input);
    if (attachError) throw attachError;
    return { value: { terminal_attachment: { runtime_incarnation: calls.incarnation, stream_capability: `capability-${calls.attaches.length}` } } };
  };
  client.terminalDetach = async input => { calls.detaches.push(input); return { value: {} }; };
  return { client, sockets, calls };
}
const terminalSubscriptions = socket => socket.sent.filter(command => command.collection === 'terminal');
const screenFrame = value => ({ kind: 'screen', id: 'terminal', collection: 'terminal', snapshot: { id: 'snapshot/screen' }, value });
const errorFrame = (code, message) => ({ kind: 'error', id: 'terminal', collection: 'terminal', code, message });

{
  // The first screen shows at once, then the terminal rides the socket with the attach capability.
  const { client, sockets, calls } = fakeClient();
  const feed = new Feed(client, noWindows, new ForegroundGate('active'), () => `action/test-${calls.attaches.length + calls.detaches.length}`, [5]);
  await settle();
  const screens = [], issues = [];
  const follow = feed.followTerminal(terminalId, { onScreen: next => screens.push(next.lines[0].text), onIssue: issue => issues.push(issue) });
  await settle();
  assert.deepEqual(screens, ['current']);
  assert.equal(calls.attaches[0].fence.runtime_incarnation, 'incarnation/one');
  assert.equal(calls.attaches[0].parameters.target_id, terminalId);
  assert.deepEqual(terminalSubscriptions(sockets[0]), [{ kind: 'subscribe', id: 'terminal', collection: 'terminal', terminal: terminalId, incarnation: 'incarnation/one', capability: 'capability-1' }]);
  sockets[0].frame(screenFrame(screen('changed')));
  assert.deepEqual(screens, ['current', 'changed']);

  // A transient error reattaches on the same socket; it never polls meanwhile.
  sockets[0].frame(errorFrame('remote-unavailable', 'owner host/two is temporarily unavailable'));
  assert.match(issues.at(-1), /reconnecting/);
  await settle();
  assert.equal(sockets.length, 1);
  assert.equal(calls.attaches.length, 2);
  assert.equal(terminalSubscriptions(sockets[0]).at(-1).capability, 'capability-2');
  sockets[0].frame(screenFrame(screen('after reattach')));
  assert.equal(issues.at(-1), '');

  // A dropped socket attaches again on the new one.
  sockets[0].drop(new Error('lost'));
  await settle();
  assert.equal(sockets.length, 2);
  assert.equal(terminalSubscriptions(sockets[1]).at(-1).capability, 'capability-3');

  // st also says stale-fence when the owner is briefly out of reach: the same incarnation is
  // attached again.
  sockets[1].frame(errorFrame('stale-fence', 'the terminal owner is not reachable'));
  assert.match(issues.at(-1), /reconnecting/);
  assert.doesNotMatch(issues.at(-1), /stale-fence/);
  await settle();
  assert.equal(calls.attaches.length, 4);
  assert.equal(terminalSubscriptions(sockets[1]).at(-1).capability, 'capability-4');

  // A terminal that really restarted is refused by the attach's own incarnation check, and
  // never reattached: input must not reach a replacement process.
  calls.incarnation = 'incarnation/two';
  sockets[1].frame(errorFrame('stale-fence', 'the terminal restarted'));
  await settle();
  assert.equal(issues.at(-1), TERMINAL_RESTARTED);
  sockets[1].drop(new Error('lost'));
  await settle();
  assert.equal(calls.attaches.length, 4, 'a restart is not reattached, even after a reconnect');
  assert.deepEqual(terminalSubscriptions(sockets[2]), []);
  follow.close();
  feed.close();
}

{
  // A refused attachment is not retried.
  const { client, sockets, calls } = fakeClient({ attachError: Object.assign(new Error('not allowed'), { response: { code: 'forbidden' } }) });
  const feed = new Feed(client, noWindows, new ForegroundGate('active'), () => 'action/test', [5]);
  await settle();
  const issues = [];
  feed.followTerminal(terminalId, { onScreen: () => {}, onIssue: issue => issues.push(issue) });
  await settle();
  await settle();
  assert.equal(calls.attaches.length, 1);
  assert.equal(issues.at(-1), 'forbidden: not allowed');
  assert.deepEqual(terminalSubscriptions(sockets[0]), []);
  feed.close();
}

{
  // After a dropped socket, a runtime now on another incarnation ends following.
  const { client, sockets, calls } = fakeClient();
  const feed = new Feed(client, noWindows, new ForegroundGate('active'), () => 'action/test', [5]);
  await settle();
  const issues = [];
  feed.followTerminal(terminalId, { onScreen: () => {}, onIssue: issue => issues.push(issue) });
  await settle();
  calls.incarnation = 'incarnation/two';
  sockets[0].drop(new Error('lost'));
  await settle();
  assert.equal(issues.at(-1), TERMINAL_RESTARTED);
  assert.equal(calls.attaches.length, 1, 'nothing attaches to the replacement process');
  // A screen from another incarnation ends following too.
  feed.close();
}

{
  // Leaving the view unsubscribes the terminal and ends the viewer record.
  const { client, sockets, calls } = fakeClient();
  const feed = new Feed(client, noWindows, new ForegroundGate('active'), () => 'action/test', [5]);
  await settle();
  const screens = [];
  const follow = feed.followTerminal(terminalId, { onScreen: next => screens.push(next.lines[0].text), onIssue: () => {} });
  await settle();
  follow.close();
  assert.deepEqual(sockets[0].sent.at(-1), { kind: 'unsubscribe', id: 'terminal' });
  await settle();
  assert.equal(calls.detaches.length, 1);
  assert.equal(calls.detaches[0].parameters.target_id, terminalId);
  assert.equal(calls.detaches[0].fence.runtime_incarnation, 'incarnation/one');
  sockets[0].frame(screenFrame(screen('late')));
  sockets[0].drop(new Error('lost'));
  await settle();
  assert.deepEqual(terminalSubscriptions(sockets[1]), [], 'a closed view never reattaches');
  assert.deepEqual(screens, ['current']);
  feed.close();
}

{
  // In the background nothing follows the terminal; the foreground attaches again.
  const { client, sockets, calls } = fakeClient();
  const attach = client.terminalAttach;
  let pendingAttach, attachRequests = 0;
  // The second attachment is still in flight when the app leaves the foreground.
  client.terminalAttach = input => ++attachRequests === 2 ? new Promise(resolve => { pendingAttach = () => resolve(attach(input)); }) : attach(input);
  const foreground = new ForegroundGate('background');
  const feed = new Feed(client, noWindows, foreground, () => 'action/test', [5]);
  const screens = [];
  const follow = feed.followTerminal(terminalId, { onScreen: next => screens.push(next.lines[0].text), onIssue: () => {} });
  await settle();
  assert.equal(calls.reads, 0, 'a view opened in the background reads nothing');

  foreground.update('active');
  await settle();
  assert.equal(terminalSubscriptions(sockets[0]).length, 1);
  sockets[0].frame(screenFrame(screen('live')));
  foreground.update('background');
  assert.equal(sockets[0].closed, true, 'leaving the foreground closes the socket');

  foreground.update('active');
  await settle();
  assert.equal(attachRequests, 2, 'the foreground attaches again');
  foreground.update('background');
  pendingAttach();
  await settle();
  assert.deepEqual(terminalSubscriptions(sockets[1]), [], 'an attachment that returns after the app left the foreground subscribes nothing');

  foreground.update('active');
  await settle();
  assert.equal(terminalSubscriptions(sockets[2]).length, 1);
  assert.deepEqual(screens, ['current', 'live'], 'the socket sends the current screen first; the view keeps the last one meanwhile');
  follow.close();
  feed.close();
}
