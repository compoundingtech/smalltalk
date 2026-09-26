import assert from 'node:assert/strict';
import { rememberBounded } from './boundedCache.ts';

const cache = new Map();
for (let index = 0; index < 24; index++) rememberBounded(cache, `session/${index}`, index, 24);
rememberBounded(cache, 'session/0', 'draft', 24);
rememberBounded(cache, 'session/24', 'new', 24);
assert.equal(cache.size, 24);
assert.equal(cache.get('session/0'), 'draft');
assert.equal(cache.has('session/1'), false);
assert.equal(cache.get('session/24'), 'new');
assert.throws(() => rememberBounded(cache, 'bad', 'bad', 0), /positive/);
