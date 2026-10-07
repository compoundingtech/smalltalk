import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { agentBranch, checkoutLabel, agentParameters, models, randomName } from './launcher.ts';

assert.equal(randomName(() => 0), 'amber-badger');
assert.match(randomName(), /^[a-z]+-[a-z]+$/);
assert.deepEqual(models('codex'), ['default', 'gpt-6-sol']);
assert.deepEqual(models('pi'), ['default']);
// A default choice is the harness's own; an empty first message is left out.
assert.deepEqual(agentParameters({ message: '  ', name: ' keen-otter ', harness: 'claude', model: 'default', effort: 'default' }), { name: 'keen-otter', harness: 'claude' });
assert.deepEqual(agentParameters({ message: 'Fix the build', name: 'keen-otter', harness: 'codex', model: 'gpt-6-sol', effort: 'high', host: 'host/willow' }),
  { name: 'keen-otter', harness: 'codex', model: 'gpt-6-sol', effort: 'high', host: 'host/willow', message: 'Fix the build' });

// The same creation and tab-context contract used by stui.
const fixture = JSON.parse(readFileSync(new URL('../../fixtures/clients/agent-launch.json', import.meta.url), 'utf8'));
for (const { name, branch } of fixture.branches) assert.equal(agentBranch(name), branch);
for (const { form, parameters } of fixture.launches) assert.deepEqual(agentParameters(form), parameters);
assert.equal(checkoutLabel(fixture.checkout), fixture.checkout_label);
assert.equal(checkoutLabel(), undefined);
