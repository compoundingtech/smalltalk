import assert from 'node:assert/strict';
import { test } from 'node:test';
import { contentReferences, loadConversationContent, contentJsonText, contentImageUri, conversationEntries, fetchedConversationEntries } from '../index.ts';

const reference = { ref: 'opaque/ref', media_type: 'application/json', reason: 'size-limit' };
const chunk = (data, offset, size, next_offset = null) => ({ kind: 'conversation-content-chunk', ref: reference.ref, media_type: reference.media_type, offset, size, data: btoa(data), next_offset });

test('assembles full original JSON from offset zero across split UTF-8 chunks', async () => {
  const original = '{"text":"héllo","view":{"diff":"full"}}';
  const bytes = new TextEncoder().encode(original);
  const binary = String.fromCharCode(...bytes);
  const offsets = [];
  const content = await loadConversationContent(reference, async (ref, offset) => {
    assert.equal(ref, reference.ref); offsets.push(offset);
    return offset === 0 ? chunk(binary.slice(0, 11), 0, bytes.length, 11) : chunk(binary.slice(11), 11, bytes.length);
  });
  assert.deepEqual(offsets, [0, 11]);
  assert.deepEqual(JSON.parse(contentJsonText(content)), JSON.parse(original));
});

test('displays payload, metadata/view and whole-body JSON without guessing a merge location', async () => {
  for (const original of [{ text: 'full payload' }, { type: 'edit', diff: 'full metadata/view' }, { blocks: [{ payload: { text: 'full body' } }], text: 'full' }]) {
    const json = JSON.stringify(original);
    const content = await loadConversationContent(reference, async () => chunk(json, 0, json.length));
    assert.deepEqual(JSON.parse(contentJsonText(content)), original);
  }
});

test('rejects broken chunk offsets, sizes, references and premature completion', async () => {
  for (const change of [{ offset: 1 }, { ref: 'wrong' }, { size: 2 }, { next_offset: 0 }, { next_offset: 2 }]) {
    await assert.rejects(loadConversationContent(reference, async () => ({ ...chunk('a', 0, 1), ...change })));
  }
  await assert.rejects(loadConversationContent(reference, async () => { throw new Error('offline'); }), /offline/);
});
test('rejects changing chunk metadata and malformed JSON or base64', async () => {
  for (const change of [{ size: 3 }, { media_type: 'text/plain' }]) {
    await assert.rejects(loadConversationContent(reference, async (_, offset) => offset === 0
      ? chunk('a', 0, 2, 1) : { ...chunk('b', 1, 2), ...change }));
  }
  await assert.rejects(loadConversationContent(reference, async () => ({ ...chunk('a', 0, 1), data: '?' })));
  assert.throws(() => contentJsonText({ bytes: new TextEncoder().encode('{'), mediaType: 'application/json' }));
});


test('discovers nested image refs and deduplicates continuation refs', () => {
  assert.deepEqual(contentReferences({ blocks: [{ continuation: reference, payload: { image_refs: [{ ref: 'image', media_type: 'application/octet-stream', reason: 'on-demand' }, reference] } }] }).map(value => value.ref), [reference.ref, 'image']);
});

test('detects passive image media from octet-stream bytes and rejects active content', () => {
  const bytes = Uint8Array.from([137, 80, 78, 71, 13, 10, 26, 10]);
  assert.equal(contentImageUri({ bytes, mediaType: 'application/octet-stream' }), 'data:image/png;base64,iVBORw0KGgo=');
  assert.throws(() => contentImageUri({ bytes: new TextEncoder().encode('<svg/>'), mediaType: 'image/svg+xml' }), /cannot be displayed/);
});

test('keeps result continuation on merged tool row and image-only content visible', () => {
  const entry = (id, type, body) => ({ id, type, body, role: 'assistant', timestamp: '2026-10-08T12:00:00Z' });
  const rows = conversationEntries([
    entry('call', 'tool_call', { call_id: 'c', name: 'read', arguments: {} }),
    entry('result', 'tool_result', { call_id: 'c', content: 'clipped', blocks: [{ continuation: reference }] }),
    entry('image', 'content', { blocks: [{ kind: 'image', continuation: { ref: 'image', media_type: 'application/octet-stream', reason: 'on-demand' } }] }),
  ], new Map());
  assert.equal(rows[0].id, 'call');
  assert.equal(rows[0].content[0].ref, reference.ref);
  assert.equal(rows[1].content[0].ref, 'image');
});

test('full tool body and clipped payload use the typed output projection beyond 400 lines', () => {
  const output = Array.from({ length: 600 }, (_, index) => `line-${index}`).join('\n');
  const source = { id: 'result', role: 'assistant', timestamp: '2026-10-08T12:00:00Z', type: 'tool_result',
    body: { call_id: 'c', status: 'success', media_type: 'text/plain', content: 'clipped',
      blocks: [{ kind: 'tool_output', payload: '[st truncated this native timeline value: size limit; 20000 bytes]', continuation: reference, view: { type: 'bash', exit_code: 0 } }] } };
  const [entry] = conversationEntries([source], new Map());
  for (const value of [output, { call_id: 'c', status: 'success', media_type: 'text/plain', content: output }]) {
    const [shown] = fetchedConversationEntries(entry, reference, value);
    assert.equal(shown.body.kind, 'tool');
    assert.ok(shown.body.output.includes('line-599'));
    assert.ok(!shown.body.output.some(line => line.includes('"content"')));
  }
});

test('metadata and view subtrees do not get guessed into a tool payload', () => {
  const source = { id: 'result', role: 'assistant', timestamp: '2026-10-08T12:00:00Z', type: 'tool_result',
    body: { call_id: 'c', status: 'success', media_type: 'text/plain', content: 'normal output',
      blocks: [{ kind: 'tool_output', payload: { body_ref: true }, metadata: { large: '[st truncated this native timeline value: size limit; 20000 bytes]' }, continuation: reference }] } };
  const [entry] = conversationEntries([source], new Map());
  assert.equal(fetchedConversationEntries(entry, reference, { fullMetadata: 'complete' }), undefined);
});
