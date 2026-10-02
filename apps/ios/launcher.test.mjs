import assert from 'node:assert/strict';
import { agentParameters, models, randomName } from './launcher.ts';

assert.equal(randomName(() => 0), 'amber-badger');
assert.match(randomName(), /^[a-z]+-[a-z]+$/);
assert.deepEqual(models('codex'), ['default', 'gpt-6-sol']);
assert.deepEqual(models('pi'), ['default']);
// A default choice is the harness's own; an empty first message is left out.
assert.deepEqual(agentParameters({ message: '  ', name: ' keen-otter ', harness: 'claude', model: 'default', effort: 'default' }), { name: 'keen-otter', harness: 'claude' });
assert.deepEqual(agentParameters({ message: 'Fix the build', name: 'keen-otter', harness: 'codex', model: 'gpt-6-sol', effort: 'high', host: 'host/willow' }),
  { name: 'keen-otter', harness: 'codex', model: 'gpt-6-sol', effort: 'high', host: 'host/willow', message: 'Fix the build' });
