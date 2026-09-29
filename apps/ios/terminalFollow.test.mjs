import assert from 'node:assert/strict';
import { followTerminal, TERMINAL_RESTARTED } from './terminalControls.ts';
import { ForegroundGate } from './foreground.ts';

const terminalId = 'terminal/agent/worker';
const screen = (text, incarnation = 'incarnation/one') => ({ runtime_incarnation: incarnation, next_sequence: 7, lines: [{ row: 0, text }] });
const settle = () => new Promise(resolve => setTimeout(resolve, 20));

function fakeClient({ incarnation = 'incarnation/one', attachError } = {}) {
  const calls = { reads: 0, attaches: [], streams: [] };
  const client = {
    terminalScreen: async id => { assert.equal(id, terminalId); calls.reads++; return { snapshot: { id: `snapshot/${calls.reads}` }, value: screen('current', incarnation) }; },
    terminalAttach: async input => {
      calls.attaches.push(input);
      if (attachError) throw attachError;
      return { value: { terminal_attachment: { runtime_incarnation: incarnation, stream_capability: `capability-${calls.attaches.length}` } } };
    },
    terminalStream: async (id, options) => {
      const stream = { options, closed: false, close() { this.closed = true; } };
      calls.streams.push(stream);
      return stream;
    },
  };
  return { client, calls };
}

{
  const { client, calls } = fakeClient();
  const screens = [], issues = [];
  let actions = 0;
  const follow = followTerminal(client, terminalId, { onScreen: next => screens.push(next.lines[0].text), onIssue: issue => issues.push(issue) }, () => `action/test-${++actions}`, new ForegroundGate('active'), [5]);
  await settle();
  assert.equal(calls.streams.length, 1, 'one screen read and one attachment open one stream');
  assert.equal(calls.attaches[0].fence.runtime_incarnation, 'incarnation/one');
  assert.equal(calls.attaches[0].parameters.target_id, terminalId);
  assert.equal(calls.streams[0].options.streamCapability, 'capability-1');
  calls.streams[0].options.onScreen({ value: screen('changed') });
  assert.deepEqual(screens, ['current', 'changed']);

  // A dropped stream reattaches and resumes from the current screen; it never polls meanwhile.
  calls.streams[0].options.onEnd(new Error('socket closed'));
  assert.match(issues.at(-1), /reconnecting/);
  await settle();
  assert.equal(calls.reads, 4, 'each open reads the screen to show it, then once more for a fresh attach fence');
  assert.equal(calls.streams.length, 2);
  assert.equal(calls.streams[1].options.streamCapability, 'capability-2');
  calls.streams[1].options.onScreen({ value: screen('after reconnect') });
  assert.equal(issues.at(-1), '');

  // A new incarnation ends following: input must not reach a replacement process.
  calls.streams[1].options.onEnd({ response: { code: 'stale-fence' }, message: 'changed incarnation' });
  assert.equal(issues.at(-1), TERMINAL_RESTARTED);
  await settle();
  assert.equal(calls.streams.length, 2, 'a stale fence is not retried');
  follow.close();
}

{
  const { client, calls } = fakeClient({ attachError: Object.assign(new Error('not allowed'), { response: { code: 'forbidden' } }) });
  const issues = [];
  followTerminal(client, terminalId, { onScreen: () => {}, onIssue: issue => issues.push(issue) }, () => 'action/test', new ForegroundGate('active'), [5]);
  await settle();
  await settle();
  assert.equal(calls.attaches.length, 1, 'a refused attachment is not retried');
  assert.equal(issues.at(-1), 'forbidden: not allowed');
}

{
  const { client, calls } = fakeClient();
  const follow = followTerminal(client, terminalId, { onScreen: () => {}, onIssue: () => {} }, () => 'action/test', new ForegroundGate('active'), [5]);
  await settle();
  follow.close();
  assert.equal(calls.streams[0].closed, true);
  calls.streams[0].options.onEnd(new Error('closed by the viewer'));
  await settle();
  assert.equal(calls.streams.length, 1, 'a closed view never reconnects');
}

{
  // In the background nothing follows the terminal; the foreground reattaches from the current screen.
  let pendingAttach, attachRequests = 0;
  const { client, calls } = fakeClient();
  const attach = client.terminalAttach;
  // The second attachment is still in flight when the app leaves the foreground.
  client.terminalAttach = input => ++attachRequests === 2 ? new Promise(resolve => { pendingAttach = () => resolve(attach(input)); }) : attach(input);
  const foreground = new ForegroundGate('background');
  const screens = [];
  const follow = followTerminal(client, terminalId, { onScreen: next => screens.push(next.lines[0].text), onIssue: () => {} }, () => 'action/test', foreground, [5]);
  await settle();
  assert.equal(calls.reads, 0, 'a view opened in the background reads nothing');

  foreground.update('active');
  await settle();
  assert.equal(calls.streams.length, 1);
  calls.streams[0].options.onScreen({ value: screen('live') });
  foreground.update('background');
  assert.equal(calls.streams[0].closed, true, 'leaving the foreground closes the stream');
  await settle();
  assert.equal(calls.streams.length, 1, 'nothing reconnects in the background');

  foreground.update('active');
  await settle();
  assert.equal(attachRequests, 2, 'the foreground attaches again');
  foreground.update('background');
  pendingAttach();
  await settle();
  assert.equal(calls.streams.length, 1, 'an attachment that returns after the app left the foreground opens no stream');

  foreground.update('active');
  await settle();
  assert.equal(calls.streams.length, 2);
  assert.equal(calls.streams[1].closed, false);
  assert.deepEqual(screens, ['current', 'live', 'current', 'current'], 'each foreground open shows the current screen first');
  follow.close();
  assert.equal(calls.streams[1].closed, true);
  foreground.update('background');
  foreground.update('active');
  await settle();
  assert.equal(calls.streams.length, 2, 'a closed view ignores the foreground');
}
