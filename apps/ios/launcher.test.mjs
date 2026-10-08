import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { checkoutLabel, randomName } from './launcher.ts';

assert.equal(randomName(() => 0), 'amber-badger');
assert.match(randomName(), /^[a-z]+-[a-z]+$/);
// The same checkout label contract used by stui.
const fixture = JSON.parse(readFileSync(new URL('../../fixtures/clients/agent-launch.json', import.meta.url), 'utf8'));
assert.equal(checkoutLabel(fixture.checkout), fixture.checkout_label);
assert.equal(checkoutLabel(), undefined);
