import assert from 'node:assert/strict';
import { parseDevLink, tabLabel, tabNamed, tabOrder, TABS } from './tabs.ts';

assert.deepEqual(TABS, ['Home', 'Agents', 'Missions', 'Fleet']);
// Usage is a screen in Fleet on the phone, so its links land there.
assert.equal(tabNamed('usage'), 'Fleet');
// Earlier names still work, in links and in a stored order.
assert.equal(tabNamed('Now'), 'Home');
assert.equal(tabNamed('chat'), 'Agents');
assert.equal(tabNamed('Control'), 'Missions');
assert.equal(tabNamed('fleet'), 'Fleet');
assert.equal(tabNamed('Worktrees'), null);
assert.deepEqual(tabOrder(['Fleet', 'Now', 'Chat', 'Control']), ['Fleet', 'Home', 'Agents', 'Missions']);
// An order stored while Usage was a tab keeps the rest of its order.
assert.deepEqual(tabOrder(['Usage', 'Fleet', 'Home', 'Agents', 'Missions']), ['Fleet', 'Home', 'Agents', 'Missions']);
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
assert.deepEqual(parseDevLink(`${scheme}terminal?id=terminal/pty/x`), { kind: 'terminal', id: 'terminal/pty/x' });
assert.equal(parseDevLink(`${scheme}terminal?id=agent/x`), null);
assert.equal(parseDevLink('not a url'), null);
// Home reads as Now on screen, as stui and `st now` name it.
assert.equal(tabLabel('Home'), 'Now');
assert.equal(tabLabel('Agents'), 'Agents');
