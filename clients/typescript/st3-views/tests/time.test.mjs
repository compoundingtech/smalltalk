import assert from 'node:assert/strict';
import { ago } from '@smalltalk/st3-views/time';

const now = Date.parse('2026-09-25T08:25:00Z');
assert.equal(ago('2026-09-25T08:24:30Z', now), '30s');
assert.equal(ago('2026-09-25T07:25:00Z', now), '1h');
assert.equal(ago('2026-09-22T08:25:00Z', now), '3d');
assert.equal(ago('not a time', now), 'unknown');
