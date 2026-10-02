import assert from 'node:assert/strict';
import { ANSWERS, isRequest, report, yesNo } from './requestView.ts';

// The same report stui's test reads (crates/stui/src/ui/screens.rs).
const text = 'The deploy finished: {"status":"failed","error":"unit st3.service did not start","commit":"8821eced","host":"willow","attempt":2,"log":"/var/log/x","duration_ms":1234,"members":["maple","cedar"],"plan":{"a":1},"notes":"long\\nnotes"}';
const shown = report(text);
assert.equal(shown.before, 'The deploy finished:');
assert.deepEqual(shown.rows.slice(0, 4).map(row => [row.key, row.value, row.tone]), [
  ['status', 'failed', 'fault'], ['error', 'unit st3.service did not start', 'fault'], ['commit', '8821eced', 'text'], ['host', 'willow', 'text'],
]);
assert.equal(shown.rows.length, 8, 'telling fields, then four others');
assert.equal(shown.more, 2);
assert.equal(shown.rows.find(row => row.key === 'notes'), undefined);
assert.equal(report('Can you look at the deploy?'), null);
assert.equal(report('{not json}'), null);

assert.ok(isRequest('person-step') && isRequest('agent-request') && !isRequest('review'));
assert.ok(yesNo('Should I merge it? ') && !yesNo('Please review the plan.'));
assert.equal(ANSWERS.nothing, 'Nothing for me to do here.');
