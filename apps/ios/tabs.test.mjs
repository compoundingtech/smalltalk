import assert from 'node:assert/strict';
import { parseDevLink, tabLabel, tabNamed, tabOrder, TABS } from './tabs.ts';
assert.deepEqual(TABS, ['Now', 'Resources', 'Fleet', 'Spaces']);
assert.equal(tabNamed('resources'), 'Resources');
// Initial navigation ignores a retired name and a saved order from an older build.
assert.equal(tabNamed('Home') ?? tabOrder(['Agents', 'Missions', 'Fleet', 'Home'])[0], 'Now');
assert.equal(tabNamed(' Now '), 'Now');
for (const retired of ['Home', 'Agents', 'Missions', 'Chat', 'Control', 'Usage', 'Glasses']) assert.equal(tabNamed(retired), null);
assert.deepEqual(tabOrder(['Fleet', 'Resources', 'Now', 'Spaces']), ['Fleet', 'Resources', 'Now', 'Spaces']);
for (const old of [['Fleet', 'Home', 'Agents', 'Missions'], ['Fleet'], ['Now', 'Now', 'Fleet', 'Spaces'], 'nonsense']) assert.deepEqual(tabOrder(old), [...TABS]);
const scheme = 'com.compoundingtech.smalltalk.starter://';
assert.deepEqual(parseDevLink(`${scheme}tab/Now`), { kind: 'tab', tab: 'Now' });
assert.deepEqual(parseDevLink(`${scheme}tab/Resources`), { kind: 'tab', tab: 'Resources' });
assert.deepEqual(parseDevLink(`${scheme}tab/Spaces`), { kind: 'tab', tab: 'Spaces' });
for (const retired of ['Home', 'Agents', 'Missions', 'Chat', 'Control', 'Usage', 'Glasses']) assert.equal(parseDevLink(`${scheme}tab/${retired}`), null);
assert.deepEqual(parseDevLink(`${scheme}session?id=session/abc&terminal=terminal/t`), { kind: 'session', id: 'session/abc', terminal: 'terminal/t' });
assert.deepEqual(parseDevLink(`${scheme}agent?id=agent/example/stui`), { kind: 'agent', id: 'agent/example/stui' });
assert.deepEqual(parseDevLink(`${scheme}mission?id=mission/x`), { kind: 'mission', id: 'mission/x' });
assert.deepEqual(parseDevLink(`${scheme}attention?id=attention/gate`), { kind: 'attention', id: 'attention/gate' });
assert.equal(parseDevLink(`${scheme}attention?id=mission/x`), null);
assert.equal(parseDevLink(`${scheme}session?id=agent/x`), null);
assert.equal(parseDevLink('not a url'), null);
for (const tab of TABS) assert.equal(tabLabel(tab), tab);

assert.equal(parseDevLink(`${scheme}tree?on=1`), null);
