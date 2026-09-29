import assert from 'node:assert/strict';
import { Feed } from './feed.ts';
import { ForegroundGate } from './foreground.ts';
import { fakeClient } from './fakeFeed.mjs';

const settle = (ms = 20) => new Promise(resolve => setTimeout(resolve, ms));
const snapshot = index => ({ id: `snapshot/host/${index}`, host_id: 'host/one', store_index: index, projection_version: 'client-projection.v0', created_at: '2026-09-29T00:00:00Z' });
const mission = (id, title = id) => ({ kind: 'mission', id, revision: `${id}@1`, title, state: 'running', runs: [], run_generations: {} });
const agent = id => ({ kind: 'agent', id, revision: `${id}@1`, name: id, state: 'running', runtime_ids: [] });

function watch() {
  const seen = { windows: {}, connection: [], errors: [], updates: 0 };
  return {
    seen,
    handlers: {
      onWindow: (name, items, hasMore, at) => { seen.updates++; seen.windows[name] = { ids: items.map(item => item.id), hasMore, snapshot: at.id }; },
      onConnection: (state, issue) => seen.connection.push(issue ? `${state}: ${issue}` : state),
      onWindowError: (name, message) => seen.errors.push(`${name}: ${message}`),
    },
  };
}
const subscribed = socket => socket.sent.filter(command => command.kind === 'subscribe').map(command => command.id);

{
  // Snapshot then changes produce ordered lists; a removal leaves the window.
  const { client, sockets } = fakeClient();
  const { seen, handlers } = watch();
  const feed = new Feed(client, handlers, new ForegroundGate('active'), () => 'action/test', [5, 40]);
  await settle();
  assert.equal(sockets.length, 1);
  assert.deepEqual(sockets[0].sent, [
    { kind: 'subscribe', id: 'attention', collection: 'attention', limit: 50 },
    { kind: 'subscribe', id: 'missions', collection: 'missions', limit: 200 },
    { kind: 'subscribe', id: 'agents', collection: 'agents', limit: 200 },
  ]);
  assert.deepEqual(seen.connection, ['connecting']);
  sockets[0].frame({ kind: 'snapshot', id: 'missions', collection: 'missions', snapshot: snapshot(1), items: [mission('mission/b'), mission('mission/a')], order: ['mission/a', 'mission/b'], has_more: true });
  assert.deepEqual(seen.windows.missions, { ids: ['mission/a', 'mission/b'], hasMore: true, snapshot: 'snapshot/host/1' });
  assert.deepEqual(seen.connection, ['connecting', 'live']);
  sockets[0].frame({ kind: 'changes', id: 'missions', collection: 'missions', snapshot: snapshot(2), upserts: [mission('mission/c')], removes: ['mission/a'], order: ['mission/c', 'mission/b'], has_more: false });
  assert.deepEqual(seen.windows.missions, { ids: ['mission/c', 'mission/b'], hasMore: false, snapshot: 'snapshot/host/2' });

  // A refused window is reported by name, and the other windows keep going.
  sockets[0].frame({ kind: 'error', id: 'attention', message: 'invalid subscription' });
  assert.deepEqual(seen.errors, ['attention: invalid subscription']);
  sockets[0].frame({ kind: 'snapshot', id: 'agents', collection: 'agents', snapshot: snapshot(3), items: [agent('agent/one')], order: ['agent/one'], has_more: false });
  assert.deepEqual(seen.windows.agents.ids, ['agent/one']);
  assert.deepEqual(seen.connection, ['connecting', 'live'], 'a socket goes live once, at its first snapshot');

  // A resync asks for that window again, and only that one.
  const before = sockets[0].sent.length;
  sockets[0].frame({ kind: 'resync', id: 'missions' });
  assert.deepEqual(sockets[0].sent.slice(before), [{ kind: 'subscribe', id: 'missions', collection: 'missions', limit: 200 }]);

  // Idle: nothing is read or sent while no frame arrives.
  const sent = sockets[0].sent.length, updates = seen.updates;
  await settle(60);
  assert.equal(sockets.length, 1);
  assert.equal(sockets[0].sent.length, sent);
  assert.equal(seen.updates, updates);
  feed.close();
  assert.equal(sockets[0].closed, true);
}

{
  // A dropped socket reconnects after the first delay and subscribes every window again; the next
  // snapshot replaces the lists. Delays grow while drops repeat and reset after a snapshot.
  const { client, sockets } = fakeClient();
  const { seen, handlers } = watch();
  const feed = new Feed(client, handlers, new ForegroundGate('active'), () => 'action/test', [5, 60]);
  await settle();
  sockets[0].frame({ kind: 'snapshot', id: 'missions', collection: 'missions', snapshot: snapshot(1), items: [mission('mission/old')], order: ['mission/old'], has_more: false });
  sockets[0].drop(new Error('daemon restarted'));
  assert.equal(seen.connection.at(-1), 'reconnecting: daemon restarted');
  await settle();
  assert.equal(sockets.length, 2, 'the first delay has passed');
  assert.deepEqual(subscribed(sockets[1]), ['attention', 'missions', 'agents']);
  sockets[1].frame({ kind: 'snapshot', id: 'missions', collection: 'missions', snapshot: snapshot(5), items: [mission('mission/new')], order: ['mission/new'], has_more: false });
  assert.deepEqual(seen.windows.missions.ids, ['mission/new']);
  // A changes frame is never applied across sockets: the new socket's snapshot is authoritative.
  sockets[0].frame({ kind: 'changes', id: 'missions', collection: 'missions', snapshot: snapshot(6), upserts: [mission('mission/ghost')], removes: [], order: ['mission/ghost'], has_more: false });
  assert.deepEqual(seen.windows.missions.ids, ['mission/new']);

  assert.equal(seen.connection.at(-1), 'live');
  sockets[1].drop(new Error('lost'));
  await settle();
  assert.equal(sockets.length, 3, 'the snapshot reset the delay');
  sockets[2].drop(new Error('lost again'));
  await settle();
  assert.equal(sockets.length, 3, 'a second drop without a snapshot waits longer');
  await settle(80);
  assert.equal(sockets.length, 4);
  // A person asking to reconnect opens a fresh socket at once.
  feed.reconnect();
  await settle(1);
  assert.equal(sockets.length, 5);
  assert.equal(sockets[3].closed, true);
  feed.close();
  sockets[4].drop(new Error('after close'));
  await settle(80);
  assert.equal(sockets.length, 5, 'a closed feed never reconnects');
}

{
  // Background closes the socket and schedules nothing; foreground opens a fresh one.
  const { client, sockets } = fakeClient();
  const { handlers } = watch();
  const foreground = new ForegroundGate('background');
  const feed = new Feed(client, handlers, foreground, () => 'action/test', [5]);
  await settle();
  assert.equal(sockets.length, 0, 'a feed started in the background opens nothing');
  foreground.update('active');
  await settle();
  assert.equal(sockets.length, 1);
  foreground.update('background');
  assert.equal(sockets[0].closed, true);
  sockets[0].drop(new Error('closed'));
  await settle(40);
  assert.equal(sockets.length, 1, 'nothing reconnects in the background');
  foreground.update('active');
  await settle();
  assert.equal(sockets.length, 2);
  assert.deepEqual(subscribed(sockets[1]), ['attention', 'missions', 'agents']);
  feed.close();
  foreground.update('background');
  foreground.update('active');
  await settle();
  assert.equal(sockets.length, 2, 'a closed feed ignores the foreground');
}

{
  // One conversation rides the socket: its page, then changes, and again after a reconnect.
  const { client, sockets } = fakeClient();
  const { handlers } = watch();
  const feed = new Feed(client, handlers, new ForegroundGate('active'), () => 'action/test', [5]);
  await settle();
  const frames = [], issues = [];
  const follow = feed.followConversation('agent/fleet/worker', { onEntries: frame => frames.push(frame), onIssue: issue => issues.push(issue) });
  assert.deepEqual(sockets[0].sent.at(-1), { kind: 'subscribe', id: 'conversation', collection: 'conversation', conversation: 'agent/fleet/worker' });
  sockets[0].frame({ kind: 'conversation', id: 'conversation', collection: 'conversation', session_id: 'session/one', replace: true, items: [{ id: 'entry/1' }], has_more: true });
  sockets[0].frame({ kind: 'conversation', id: 'conversation', collection: 'conversation', session_id: 'session/one', replace: false, items: [{ id: 'entry/2' }] });
  assert.deepEqual(frames.map(frame => [frame.replace, frame.items.map(item => item.id), frame.hasMore]), [[true, ['entry/1'], true], [false, ['entry/2'], false]]);
  sockets[0].frame({ kind: 'error', id: 'conversation', collection: 'conversation', code: 'remote-unavailable', message: 'owner host/two is temporarily unavailable' });
  assert.equal(issues.at(-1), 'remote-unavailable: owner host/two is temporarily unavailable');
  sockets[0].drop(new Error('lost'));
  await settle();
  assert.ok(subscribed(sockets[1]).includes('conversation'), 'a reconnect follows the conversation again');
  follow.close();
  assert.deepEqual(sockets[1].sent.at(-1), { kind: 'unsubscribe', id: 'conversation' });
  sockets[1].drop(new Error('lost'));
  await settle();
  assert.ok(!subscribed(sockets[2]).includes('conversation'), 'a closed conversation is not followed again');
  feed.close();
}
