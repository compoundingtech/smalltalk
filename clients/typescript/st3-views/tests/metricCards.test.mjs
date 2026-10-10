import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { metricCards } from '@smalltalk/st3-views/metricCards';

// crates/st-surface checks the same vectors, so stui, Fractal and the web kit draw the same cards.
const vectors = JSON.parse(readFileSync(new URL('../../../../fixtures/clients/metric-cards.json', import.meta.url), 'utf8'));
assert.ok(vectors.length >= 4);
for (const vector of vectors) {
  assert.deepEqual(metricCards(vector.machines, vector.facts, vector.now_ms), vector.cards, vector.name);
}
