import assert from 'node:assert/strict';
import { parseDevLink, tabNamed, tabOrder, TABS } from './tabs.ts';

assert.deepEqual(TABS, ['Home', 'Agents', 'Missions', 'Fleet']);
// Earlier names still work, in links and in a stored order.
assert.equal(tabNamed('Now'), 'Home');
assert.equal(tabNamed('chat'), 'Agents');
assert.equal(tabNamed('Control'), 'Missions');
assert.equal(tabNamed('fleet'), 'Fleet');
assert.equal(tabNamed('Worktrees'), null);
assert.deepEqual(tabOrder(['Fleet', 'Now', 'Chat', 'Control']), ['Fleet', 'Home', 'Agents', 'Missions']);
assert.deepEqual(tabOrder(['Fleet']), ['Fleet', 'Home', 'Agents', 'Missions']);
assert.deepEqual(tabOrder('nonsense'), [...TABS]);

const scheme = 'com.compoundingtech.smalltalk.starter://';
assert.deepEqual(parseDevLink(`${scheme}tab/Now`), { kind: 'tab', tab: 'Home' });
assert.deepEqual(parseDevLink(`${scheme}tab/Agents`), { kind: 'tab', tab: 'Agents' });
assert.deepEqual(parseDevLink(`${scheme}session?id=session/abc&terminal=terminal/t`), { kind: 'session', id: 'session/abc', terminal: 'terminal/t' });
assert.deepEqual(parseDevLink(`${scheme}session?id=session/abc`), { kind: 'session', id: 'session/abc' });
assert.deepEqual(parseDevLink(`${scheme}agent?id=agent/example/stui`), { kind: 'agent', id: 'agent/example/stui' });
assert.deepEqual(parseDevLink(`${scheme}mission?id=mission/x`), { kind: 'mission', id: 'mission/x' });
assert.deepEqual(parseDevLink(`${scheme}scroll?y=800`), { kind: 'scroll', y: 800 });
assert.deepEqual(parseDevLink(`${scheme}tree?on=0`), { kind: 'tree', on: false });
assert.equal(parseDevLink(`${scheme}session?id=agent/x`), null);
assert.equal(parseDevLink('not a url'), null);
