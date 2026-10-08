import assert from 'node:assert/strict';
import { Feed, shouldProbe } from './feed.ts';
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
  // A pending source keeps its delivered rows and says they are stale. Ready changes
  // recover on the held subscription, canceling the pending resubscription timer.
  const { client, sockets } = fakeClient();
  const { seen, handlers } = watch();
  const feed = new Feed(client, handlers, new ForegroundGate('active'), () => 'action/test', [100], 40);
  await settle();
  const socket = sockets[0];
  socket.frame({ kind: 'snapshot', id: 'agents', collection: 'agents', snapshot: snapshot(1), items: [agent('agent/amber'), agent('agent/blue')], order: ['agent/amber', 'agent/blue'], has_more: false });
  const before = socket.sent.length;
  const pending = { kind: 'resync', id: 'agents', collection: 'agents', code: 'internal', message: 'collection source is unavailable; held rows are stale until readiness returns', retryable: true };
  socket.frame(pending);
  socket.frame(pending);
  assert.deepEqual(seen.windows.agents.ids, ['agent/amber', 'agent/blue'], 'resync preserves the last delivered rows');
  assert.equal(seen.errors.length, 0, 'a moment of unavailability is not reported yet');
  await settle(60);
  assert.equal(seen.errors.length, 1, 'one issue per unavailable interval, once it lasted');
  assert.match(seen.errors[0], /agents: .*held rows are stale/);
  socket.frame({ kind: 'changes', id: 'agents', collection: 'agents', snapshot: snapshot(2), upserts: [agent('agent/coral')], removes: ['agent/amber'], order: ['agent/coral', 'agent/blue'], has_more: false });
  assert.deepEqual(seen.windows.agents.ids, ['agent/coral', 'agent/blue']);
  await settle(170);
  assert.equal(socket.sent.length, before, 'Ready changes cancel resubscription');
  socket.frame(pending);
  await settle(60);
  assert.equal(seen.errors.length, 2, 'a later unavailable interval is reported again');
  // A blip a write causes (unavailable, then changes at once) is never reported.
  socket.frame({ kind: 'changes', id: 'agents', collection: 'agents', snapshot: snapshot(3), upserts: [agent('agent/coral')], removes: [], order: ['agent/coral', 'agent/blue'], has_more: false });
  socket.frame(pending);
  socket.frame({ kind: 'changes', id: 'agents', collection: 'agents', snapshot: snapshot(4), upserts: [agent('agent/coral')], removes: [], order: ['agent/coral', 'agent/blue'], has_more: false });
  await settle(60);
  assert.equal(seen.errors.length, 2, 'a blip that recovered at once says nothing');
  feed.close();
}

{
  // Snapshot then changes produce ordered lists; a removal leaves the window.
  const { client, sockets } = fakeClient();
  const { seen, handlers } = watch();
  const feed = new Feed(client, handlers, new ForegroundGate('active'), () => 'action/test', [5, 40]);
  await settle();
  assert.equal(sockets.length, 1);
  assert.deepEqual(sockets[0].sent, [
    { kind: 'subscribe', id: 'attention', collection: 'attention', limit: 200 },
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
  assert.deepEqual(seen.errors, ['attention: invalid subscription · trying again']);
  // st stopped sending it, so it is asked for again after the backoff, said only once.
  await new Promise(resolve => setTimeout(resolve, 20));
  assert.deepEqual(sockets[0].sent.at(-1), { kind: 'subscribe', id: 'attention', collection: 'attention', limit: sockets[0].sent.find(sent => sent.id === 'attention').limit });
  sockets[0].frame({ kind: 'error', id: 'attention', message: 'invalid subscription' });
  assert.equal(seen.errors.length, 1, 'a failure that keeps happening is said once');
  // Once it loads, nothing more is scheduled for it.
  sockets[0].frame({ kind: 'snapshot', id: 'attention', collection: 'attention', snapshot: snapshot(4), items: [], order: [], has_more: false });
  sockets[0].frame({ kind: 'snapshot', id: 'agents', collection: 'agents', snapshot: snapshot(3), items: [agent('agent/one')], order: ['agent/one'], has_more: false });
  assert.deepEqual(seen.windows.agents.ids, ['agent/one']);
  assert.deepEqual(seen.connection, ['connecting', 'live'], 'a socket goes live once, at its first snapshot');

  // A resync asks for that window again, and only that one; after a wait, so a resync that
  // keeps coming is not a loop and many clients do not ask at the same instant.
  const before = sockets[0].sent.length;
  sockets[0].frame({ kind: 'resync', id: 'missions' });
  assert.deepEqual(sockets[0].sent.slice(before), [], 'not at once');
  await settle(60);
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
  const follow = feed.followConversation('agent/example/worker', { onEntries: frame => frames.push(frame), onIssue: issue => issues.push(issue) });
  assert.deepEqual(sockets[0].sent.at(-1), { kind: 'subscribe', id: 'conversation', collection: 'conversation', conversation: 'agent/example/worker' });
  sockets[0].frame({ kind: 'conversation', id: 'conversation', collection: 'conversation', session_id: 'session/one', replace: true, items: [{ id: 'entry/1' }], has_more: true });
  sockets[0].frame({ kind: 'conversation', id: 'conversation', collection: 'conversation', session_id: 'session/one', replace: false, items: [{ id: 'entry/2' }] });
  assert.deepEqual(frames.map(frame => [frame.replace, frame.items.map(item => item.id), frame.hasMore]), [[true, ['entry/1'], true], [false, ['entry/2'], false]]);
  sockets[0].frame({ kind: 'error', id: 'conversation', collection: 'conversation', code: 'remote-unavailable', message: 'owner host/two is temporarily unavailable' });
  assert.equal(issues.at(-1), 'two cannot be reached right now · trying again');
  // An ended subscription is asked for again after the backoff.
  const before = sockets[0].sent.length;
  await new Promise(resolve => setTimeout(resolve, 20));
  assert.deepEqual(sockets[0].sent.slice(before), [{ kind: 'subscribe', id: 'conversation', collection: 'conversation', conversation: 'agent/example/worker' }]);
  // A page that expired under a busy store retries quietly at first.
  const quiet = issues.length;
  sockets[0].frame({ kind: 'conversation', id: 'conversation', collection: 'conversation', session_id: 'session/one', replace: true, items: [{ id: 'entry/1' }] });
  sockets[0].frame({ kind: 'error', id: 'conversation', collection: 'conversation', code: 'page-cursor-expired', message: 'the snapshot moved' });
  assert.equal(issues.length, quiet + 1, 'only the clearing of the old issue');
  await new Promise(resolve => setTimeout(resolve, 20));
  assert.equal(sockets[0].sent.at(-1).collection, 'conversation');
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

{
  // st keeps the subscription while the owner's host is away and says why: the copy shown is
  // stale, and the phone does not ask again itself.
  const { client, sockets } = fakeClient();
  const { handlers } = watch();
  const feed = new Feed(client, handlers, new ForegroundGate('active'), () => 'action/test', [5]);
  await settle();
  const issues = [];
  feed.followConversation('agent/example/worker', { onEntries: () => {}, onIssue: issue => issues.push(issue) });
  sockets[0].frame({ kind: 'conversation', id: 'conversation', collection: 'conversation', session_id: 'session/one', replace: true, items: [{ id: 'entry/1' }] });
  const before = sockets[0].sent.length;
  sockets[0].frame({ kind: 'resync', id: 'conversation', collection: 'conversation', retryable: true, code: 'remote-unavailable', message: 'owner host/two is temporarily unavailable; cached data remains usable' });
  assert.equal(issues.at(-1), 'two cannot be reached right now · trying again');
  await new Promise(resolve => setTimeout(resolve, 20));
  assert.equal(sockets[0].sent.length, before, 'st retries; the phone does not');
  // Once st reaches it again, the issue clears.
  sockets[0].frame({ kind: 'conversation', id: 'conversation', collection: 'conversation', session_id: 'session/one', replace: false, items: [{ id: 'entry/2' }] });
  assert.equal(issues.at(-1), '');
  feed.close();
}

{
  // st keeps no start for this transcript: the phone says st's reason once and stops asking.
  const { client, sockets } = fakeClient();
  const { handlers } = watch();
  const feed = new Feed(client, handlers, new ForegroundGate('active'), () => 'action/test', [5]);
  await settle();
  const issues = [];
  feed.followConversation('agent/example/worker', { onEntries: () => {}, onIssue: issue => issues.push(issue) });
  const before = sockets[0].sent.length;
  const reason = 'the retained transcript starts after its first entries; st cannot show it from the start';
  sockets[0].frame({ kind: 'error', id: 'conversation', collection: 'conversation', code: 'timeline-history-incomplete', message: reason });
  assert.equal(issues.at(-1), reason, 'st\'s own words, without "trying again"');
  await new Promise(resolve => setTimeout(resolve, 20));
  assert.equal(sockets[0].sent.length, before, 'not asked for again');
  feed.close();
}

{
  // The person's glasses ride the socket while followed: a snapshot, changes, and again after a
  // reconnect; nothing once closed.
  const { client, sockets } = fakeClient();
  const { handlers } = watch();
  const feed = new Feed(client, handlers, new ForegroundGate('active'), () => 'action/test', [5]);
  await settle();
  const seen = [], issues = [];
  const follow = feed.followGlasses({ onGlasses: glasses => seen.push(glasses.map(glass => glass.body.name)), onIssue: issue => issues.push(issue) });
  assert.deepEqual(sockets[0].sent.at(-1), { kind: 'subscribe', id: 'glasses', collection: 'glasses', limit: 100 });
  const glass = (id, name) => ({ id: `glass/person/avery/${id}`, kind: 'glass', revision: 'r1', updated_at: '', deleted: false, body: { name, tabs: [] } });
  const snapshot = { id: 'snapshot/1', host_id: 'host/one', store_index: 1, projection_version: '1', created_at: '' };
  sockets[0].frame({ kind: 'snapshot', id: 'glasses', collection: 'glasses', snapshot, items: [glass('a', 'main')], order: ['glass/person/avery/a'], has_more: false });
  sockets[0].frame({ kind: 'changes', id: 'glasses', collection: 'glasses', snapshot, upserts: [glass('b', 'review')], removes: [], order: ['glass/person/avery/a', 'glass/person/avery/b'], has_more: false });
  assert.deepEqual(seen, [['main'], ['main', 'review']]);
  sockets[0].frame({ kind: 'error', id: 'glasses', collection: 'glasses', code: 'forbidden', message: 'this device lacks read.glasses' });
  assert.equal(issues.at(-1), 'not allowed: this device lacks read.glasses');
  sockets[0].drop(new Error('lost'));
  await settle();
  assert.ok(subscribed(sockets[1]).includes('glasses'), 'a reconnect follows the glasses again');
  follow.close();
  assert.deepEqual(sockets[1].sent.at(-1), { kind: 'unsubscribe', id: 'glasses' });
  feed.close();
}


// st is asked nothing while the stream speaks; only a quiet stream is probed (Nathan, 2026-10-06).
assert.equal(shouldProbe(1_000, 5_000), false);
assert.equal(shouldProbe(1_000, 10_999), false);
assert.equal(shouldProbe(1_000, 11_000), true);

{
  // The missions window is followed only when asked: not subscribed at the start, subscribed when a
  // missions screen shows, and left (its rows dropped) when none does.
  const { client, sockets } = fakeClient();
  const { handlers } = watch();
  const stopped = [];
  const feed = new Feed(client, { ...handlers, onWindowStopped: name => stopped.push(name) }, new ForegroundGate('active'), () => 'action/test', [100], 1000, false);
  await settle();
  const socket = sockets[0];
  assert.ok(!subscribed(socket).includes('missions'), 'not followed at the start');
  assert.ok(subscribed(socket).includes('agents') && subscribed(socket).includes('attention'));
  feed.setMissions(true);
  assert.ok(subscribed(socket).includes('missions'), 'followed once a missions screen shows');
  feed.setMissions(false);
  assert.ok(socket.sent.some(command => command.kind === 'unsubscribe' && command.id === 'missions'), 'left when none shows');
  assert.deepEqual(stopped, ['missions']);
  feed.setMissions(false);
  assert.deepEqual(stopped, ['missions'], 'leaving twice says nothing more');
  feed.close();
}

{
  // Screens ask and give back; the window stays for a grace period after the last one leaves.
  const { client, sockets } = fakeClient();
  const { handlers } = watch();
  const stopped = [];
  const feed = new Feed(client, { ...handlers, onWindowStopped: name => stopped.push(name) }, new ForegroundGate('active'), () => 'action/test', [100], 1000, false, 40);
  await settle();
  const socket = sockets[0];
  const first = feed.watchMissions();
  const second = feed.watchMissions();
  assert.equal(subscribed(socket).filter(id => id === 'missions').length, 1, 'subscribed once for two screens');
  first();
  first();
  await settle(80);
  assert.deepEqual(stopped, [], 'one screen still shows missions');
  second();
  await settle(10);
  assert.deepEqual(stopped, [], 'kept through the grace period');
  const again = feed.watchMissions();
  await settle(80);
  assert.deepEqual(stopped, [], 'asked again within the grace: still followed');
  again();
  await settle(80);
  assert.deepEqual(stopped, ['missions'], 'left once the grace ended');
  feed.close();
}

