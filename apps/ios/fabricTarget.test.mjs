import assert from 'node:assert/strict';
import { buildFabricDefault, fabricGatewayId, isFabricGatewayId, decodeFabricTarget, encodeFabricTarget, fabricTarget, fabricTargetFromQuery, fabricTargetFromText, fabricTargetLabel, sameFabricTarget } from './fabricTarget.ts';

const node = 'AB'.repeat(32);
const lower = node.toLowerCase();
assert.deepEqual(fabricTarget({ node, service: 'st3-client/demo' }), { node: lower, service: 'st3-client/demo' });
assert.deepEqual(fabricTarget({ node, service: 'st3-client/demo', address: '{"relay":"x"}' }), { node: lower, service: 'st3-client/demo', address: '{"relay":"x"}' });

// Anything off about a target refuses it: a short node, a repository service, a bad service name, a hint that is not JSON.
for (const bad of [
  { node: 'abc', service: 'st3-client/demo' },
  { node, service: '' },
  { node, service: 'git/repo' },
  { node, service: 'a\nb' },
  { node, service: 'x'.repeat(256) },
  { node, service: 'st3-client/demo', address: 'not json' },
  { node, service: 'st3-client/demo', address: '"a string"' },
  { node, service: 'st3-client/demo', address: JSON.stringify({ pad: 'x'.repeat(3000) }) },
  { service: 'st3-client/demo' },
  { node: 42, service: 'st3-client/demo' },
]) assert.equal(fabricTarget(bad), null, JSON.stringify(bad).slice(0, 60));

// A query string, as a link or a build carries it.
assert.deepEqual(fabricTargetFromQuery(`node=${node}&service=st3-client%2Fdemo`), { node: lower, service: 'st3-client/demo' });
assert.deepEqual(fabricTargetFromQuery(`?node=${node}&service=s&addr=${encodeURIComponent('{"a":1}')}`), { node: lower, service: 's', address: '{"a":1}' });
assert.equal(fabricTargetFromQuery(''), null);
assert.equal(fabricTargetFromQuery(undefined), null);
assert.equal(fabricTargetFromQuery('node=zz&service=s'), null);

// A repository build carries nothing: no target, so fabric is off unless the person adds one.
assert.equal(buildFabricDefault(undefined), null);
assert.equal(buildFabricDefault(`node=${node}&service=st3-client%2Fdemo`)?.service, 'st3-client/demo');

// Stored and read back; a damaged or no longer valid record reads as none.
const target = { node: lower, service: 'st3-client/demo', address: '{"a":1}' };
assert.deepEqual(decodeFabricTarget(encodeFabricTarget(target)), target);
assert.equal(decodeFabricTarget('{'), null);
assert.equal(decodeFabricTarget(JSON.stringify({ node: 'short', service: 's' })), null);
assert.equal(decodeFabricTarget(null), null);

// The address hint does not make a different target; a different service does.
assert.ok(sameFabricTarget(target, { node: lower, service: 'st3-client/demo' }));
assert.ok(!sameFabricTarget(target, { node: lower, service: 'other' }));
assert.ok(!sameFabricTarget(target, null));
assert.equal(fabricTargetLabel(target), `st3-client/demo on ${lower.slice(0, 8)}…`);

// What a person pastes: the bare query, or a whole link that carries it.
assert.deepEqual(fabricTargetFromText(`  node=${node}&service=s  `), { node: lower, service: 's' });
assert.deepEqual(fabricTargetFromText(`com.example.app://fabric?node=${node}&service=s`), { node: lower, service: 's' });
assert.equal(fabricTargetFromText('hello'), null);

// A device paired over fabric alone is identified by its target; that identity is never a route.
assert.equal(fabricGatewayId(target), `fabric://${lower}/st3-client%2Fdemo`);
assert.ok(isFabricGatewayId(fabricGatewayId(target)));
assert.ok(!isFabricGatewayId('http://100.64.1.2:8443') && !isFabricGatewayId(''));

// ---- The ordered member list, per-member pairing, and the old single-target key an older app build reads.
{
  const { MAX_FABRIC_MEMBERS, fabricMembers, memberName, memberPairing, memberProfileKey, readFabricMembers, storedFabricMembers, walkableMembers } = await import('./fabricTarget.ts');
  const a = { node: 'a'.repeat(64), service: 'st3-client/demo', name: 'home', device: true };
  const b = { node: 'b'.repeat(64), service: 'st3-client/demo', name: 'office', address: '{"a":1}' };
  const c = { node: 'c'.repeat(64), service: 'st3-client/demo', name: 'spare' };
  const legacyTarget = encodeFabricTarget({ node: lower, service: 'st3-client/demo' });

  // A single target an older app saved reads as a list of one that uses this device's own pairing, as it does today.
  assert.deepEqual(readFabricMembers(null, legacyTarget), [{ node: lower, service: 'st3-client/demo', name: 'member 1', device: true }]);
  assert.deepEqual(readFabricMembers(null, null), []);

  // The new app's list keeps the device's member under the old key, exactly as an older app reads it.
  const stored = storedFabricMembers([a, b, c]);
  assert.deepEqual(decodeFabricTarget(stored.target), { node: a.node, service: a.service }, 'an older build reads the device member');
  assert.deepEqual(readFabricMembers(stored.members, stored.target), [a, b, c], 'the new app reads the whole list back, in order');
  assert.deepEqual(storedFabricMembers([]), { members: null, target: null }, 'an empty list removes both');
  // Reordered so a member paired on its own is first: the old key still names the device's member, never one
  // whose credential is its own (an older build would send this device's bearer to it).
  const reordered = storedFabricMembers([b, c, a]);
  assert.deepEqual(decodeFabricTarget(reordered.target), { node: a.node, service: a.service });
  assert.deepEqual(readFabricMembers(reordered.members, reordered.target), [b, c, a]);
  // No device member: the old key is removed, and an older build uses its saved gateway.
  assert.equal(storedFabricMembers([b, c]).target, null);
  assert.deepEqual(readFabricMembers(storedFabricMembers([b, c]).members, null), [b, c]);
  // The device member's address hint goes to the old key too.
  assert.deepEqual(decodeFabricTarget(storedFabricMembers([{ ...b, device: true }]).target), { node: b.node, service: b.service, address: b.address });

  // A list edited by hand (moved, renamed, removed) is stored and read back as edited.
  const edited = storedFabricMembers([{ ...c, name: 'backup' }, a]);
  assert.deepEqual(readFabricMembers(edited.members, edited.target), [{ ...c, name: 'backup' }, a]);

  // An older build replaced its target after this app saved its list: the new target is the device's member,
  // in the old one's place, under a name of its own (a different machine does not inherit "home").
  const replaced = readFabricMembers(reordered.members, legacyTarget);
  assert.deepEqual(replaced.map(m => [m.name, !!m.device]), [['office', false], ['spare', false], ['member 3', true]]);
  assert.equal(replaced[2].node, lower);
  // It chose a member already in the list: that member is the device's now, under its own name, once.
  assert.deepEqual(readFabricMembers(stored.members, encodeFabricTarget(c)).map(m => [m.name, !!m.device]), [['home', false], ['office', false], ['spare', true]]);
  // It forgot its target: the device's member is forgotten too; members paired on their own stay.
  assert.deepEqual(readFabricMembers(stored.members, null), [b, c]);
  // A damaged list falls back to the old key.
  assert.equal(readFabricMembers('{', legacyTarget).length, 1);

  // Only one member uses this device's pairing; invalid entries, repeats and repeated names are dropped or renamed.
  assert.deepEqual(fabricMembers([a, { ...c, device: true }]).map(m => !!m.device), [true, false]);
  assert.deepEqual(fabricMembers([a, { node: 'short', service: 's', name: 'x' }, { ...a, name: 'again' }, { ...c, name: 'HOME' }]).map(m => m.name), ['home', 'member 2']);
  assert.equal(memberName('  home\nlab  '), 'home lab');
  assert.equal(memberName('x'.repeat(60)).length, 40);
  assert.equal(memberName(7), '');
  const many = Array.from({ length: 12 }, (_, i) => ({ node: i.toString(16).padStart(2, '0').repeat(32), service: 's', name: `m${i}` }));
  assert.equal(fabricMembers(many).length, MAX_FABRIC_MEMBERS);
  assert.deepEqual(fabricMembers('nope'), []);

  // Each member's own pairing has its own Keychain entry, named by member and service only.
  assert.equal(memberProfileKey(c), `st3.fabric.profile.${c.node}.${Buffer.from(c.service).toString('hex')}`);
  assert.match(memberProfileKey(c), /^[\w.-]+$/, 'a valid secure-store key');
  assert.notEqual(memberProfileKey(c), memberProfileKey({ ...c, service: 'other' }));
  // The walk dials only members this phone is paired with, in the person's order; each credential goes only to its member.
  const own = new Set([memberProfileKey(c)]);
  assert.deepEqual([a, b, c].map(m => memberPairing(m, true, own)), ['device', 'none', 'own']);
  assert.deepEqual(walkableMembers([b, c, a], true, own).map(m => m.name), ['spare', 'home'], 'office is skipped until it is paired');
  assert.deepEqual(walkableMembers([a, b, c], false, own).map(m => m.name), ['spare'], 'an unpaired device has no credential for its member');

  // Each credential goes only to the member it was paired with.
  const { credentialFor } = await import('./fabricTarget.ts');
  assert.equal(credentialFor(false, null, null, 'device-bearer'), 'device-bearer', 'the saved gateway gets this device\'s own');
  assert.equal(credentialFor(true, a, null, 'device-bearer'), 'device-bearer', 'so does the member that uses this device\'s pairing');
  assert.equal(credentialFor(true, c, 'spare-bearer', 'device-bearer'), 'spare-bearer', 'a member paired on its own gets its own');
  assert.equal(credentialFor(true, b, null, 'device-bearer'), null, 'a member not paired yet gets none, never this device\'s');
  assert.equal(credentialFor(true, null, 'spare-bearer', 'device-bearer'), null, 'a member that cannot be named gets none');

  // A build's default target is the member that uses this device's own pairing (it was the only target before the
  // list), so a phone paired through its form has it to walk; it is not a member waiting for its own pairing.
  const { builtFabricMembers } = await import('./fabricTarget.ts');
  const built = buildFabricDefault(`node=${'d'.repeat(64)}&service=st3-client%2Fdemo`);
  const opened = builtFabricMembers(built);
  assert.deepEqual(opened.map(m => [m.name, m.device]), [['member 1', true]]);
  assert.deepEqual(walkableMembers(opened, true, new Set()).map(m => m.name), ['member 1'], 'a paired phone dials the built target at once');
  assert.deepEqual(builtFabricMembers(null), []);
}
