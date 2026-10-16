import assert from 'node:assert/strict';
import { keyBetween, folderCreate, placeResource, moveFolder, folderLocation, folderName, sidebarArrangement, sidebarRetired, sidebarSubject, uuid7 } from './sidebarEdits.ts';
import { ClientError } from '../../clients/typescript/st3-client/index.ts';
const valid = key => /^[A-Za-z][0-9A-Za-z]+$/.test(key) && key.length <= 128 && key.length >= (key[0] >= 'a' ? key.charCodeAt(0) - 95 : 92 - key.charCodeAt(0)) && (key.length === (key[0] >= 'a' ? key.charCodeAt(0) - 95 : 92 - key.charCodeAt(0)) || !key.endsWith('0'));
for (const [low, high] of [[null, null], [null, 'a0'], ['a0', null], ['Zz', 'a0'], ['a0', 'a1'], ['a0', 'a0V'], ['a0V', 'a1'], ['azz', 'b00'], ['Z0', 'Z1'], ['Yzz', 'Z0'], [null, 'Z0']]) {
  const key = keyBetween(low, high); assert.ok(valid(key), key); assert.ok(low === null || low < key); assert.ok(high === null || key < high);
}
let high = 'a1';
for (let i = 0; i < 600; i++) { const key = keyBetween('a0', high); assert.ok('a0' < key && key < high && valid(key)); high = key; }
const reg = value => ({ value, revision: 'claim/a' });
const a = { body: { folders: { first: { position: reg({ key: 'a0', parent: null }), tombstone: null }, second: { position: reg({ key: 'a1', parent: null }), tombstone: null } }, placements: { 'agent/atlas': reg({ folder: 'first', key: 'a0' }), 'agent/dormant': reg({ folder: 'first', key: 'a1' }) } } };
assert.equal(folderCreate(a, 'new', ' New ')[0].name, 'New');
assert.ok(folderCreate(a, 'new', 'New')[0].key > 'a1');
assert.ok(placeResource(a, 'agent/new', 'first')[0].key > 'a1');
assert.ok(placeResource(a, 'agent/dormant', 'first', 'agent/atlas')[0].key < 'a0');
assert.ok(moveFolder(a, 'second', 'first')[0].key < 'a0');
assert.equal(folderLocation({ ...a, resolved: { folders: { 'agent/atlas': null } } }, 'agent/atlas'), null);
assert.equal(a.body.placements['agent/atlas'].value.folder, 'first');
assert.match(uuid7(1700000000000, new Uint8Array(10).fill(255)), /^018bcfe5-6800-7fff-bfff-ffffffffffff$/);
const named = (uuid, name, deleted = false) => ({ kind: 'arrangement', id: `arrangement/person/avery/${uuid}`, owner: 'person/avery', deleted, body: { version: 1, name: { value: name, revision: 'claim/1' }, folders: {}, placements: {} } });
const reserved = sidebarSubject('person/avery');
assert.equal(reserved, 'arrangement/person/avery/00000000-0000-7000-8000-000000000001');
assert.equal(sidebarArrangement([named('019a0000-0000-7000-8000-000000000009', 'Desk')]), undefined, 'only a Sidebar is the Sidebar');
const legacy = [named('019a0000-0000-7000-8000-000000000003', 'Sidebar'), named('019a0000-0000-7000-8000-000000000002', 'Sidebar')];
assert.equal(sidebarArrangement(legacy).id, legacy[1].id, 'the oldest legacy Sidebar');
const renamed = { ...named('00000000-0000-7000-8000-000000000001', 'Renamed') };
assert.equal(sidebarArrangement([...legacy, renamed]).id, reserved, 'the reserved subject wins whatever its name');
assert.equal(sidebarArrangement([...legacy, { ...renamed, deleted: true }]).id, legacy[1].id);

// Concurrent devices can choose the same key. One atomic rekey keeps dormant placements and touches each register once.
const colliding = structuredClone(a); colliding.body.placements['agent/dormant'].value.key = 'a0';
const rekeyed = placeResource(colliding, 'agent/new', 'first', 'agent/dormant');
assert.equal(new Set(rekeyed.map(op => op.subject)).size, rekeyed.length);
assert.equal(rekeyed.length, 3);
const keys = Object.fromEntries(rekeyed.map(op => [op.subject, op.key]));
assert.ok(keys['agent/atlas'] < keys['agent/new'] && keys['agent/new'] < keys['agent/dormant']);
assert.ok(rekeyed.every(op => valid(op.key)));
const unfiled = placeResource(a, 'agent/new', null, 'agent/unplaced');
assert.equal(unfiled.length, 2);
assert.ok(unfiled.find(op => op.subject === 'agent/new').key < unfiled.find(op => op.subject === 'agent/unplaced').key);

for (const name of ["", "  ", "a\n", "a\u0085b", "é".repeat(129)]) assert.throws(() => folderName(name));
assert.equal(folderName(" é "), "é");
assert.equal(folderName("é".repeat(128)).length, 128);

// A retired Sidebar is learned from st's refusal to create it again.
const refused = code => new ClientError({ api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', request_id: 'request/x', code, message: 'm', retryable: false, details: {} }, 409);
assert.equal(sidebarRetired(refused('arrangement-retired')), true);
assert.equal(sidebarRetired(refused('arrangement-exists')), false);
assert.equal(sidebarRetired(new Error('offline')), false);
