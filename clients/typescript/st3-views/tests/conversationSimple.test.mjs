import assert from 'node:assert/strict';
import { bundleId, simplify } from '@smalltalk/st3-views/conversationSimple';

// The same conversation st3-conversation-ui's simplified test draws.
const entry = (id, body) => ({ id, at: '09:00', body });
const call = (id, title, state) => entry(id, { kind: 'tool', title, state, output: ['line one', 'line two'] });
const entries = [
  entry('a', { kind: 'user', text: 'check the build' }),
  call('t1', '$ cargo build', 'ok'),
  call('t2', '$ cargo test', 'failed'),
  call('t3', '$ cargo test -p stui', 'ok'),
  entry('b', { kind: 'assistant', text: 'One test failed; fixed.' }),
  call('t4', '$ cat src/main.rs', 'ok'),
];

const folded = simplify(entries, new Set());
assert.deepEqual(folded.map(row => row.kind), ['entry', 'bundle', 'entry', 'call']);
const bundle = folded[1];
assert.equal(bundle.id, bundleId('t1'));
assert.deepEqual([bundle.calls.length, bundle.ok, bundle.failed, bundle.running, bundle.last, bundle.open], [3, 2, 1, 0, '$ cargo test -p stui', false]);
assert.equal(folded[3].tool.title, '$ cat src/main.rs');

// Opened, a run lists its calls after its own line.
const opened = simplify(entries, new Set([bundleId('t1')]));
assert.deepEqual(opened.map(row => row.kind), ['entry', 'bundle', 'call', 'call', 'call', 'entry', 'call']);
assert.equal(opened[1].open, true);

// Content previews and recoverable errors are not swallowed by a run of tool calls.
{
  const rows = simplify([
    call('before', '$ echo before', 'ok'),
    call('write', 'write demo · 2 lines · 10 bytes', 'ok'),
    call('error', 'assistant error · recovered · retried', 'ok'),
    call('compaction', 'compaction · 100 → 20 tokens', 'ok'),
    call('after', '$ echo after', 'ok'),
  ], new Set());
  assert.deepEqual(rows.map(row => row.kind), ['call', 'entry', 'entry', 'entry', 'call']);
}
