import assert from 'node:assert/strict';
import { buildFabricDefault, decodeFabricTarget, encodeFabricTarget, fabricTarget, fabricTargetFromQuery, fabricTargetFromText, fabricTargetLabel, sameFabricTarget } from './fabricTarget.ts';

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
