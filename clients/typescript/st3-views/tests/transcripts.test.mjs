import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { conversationEntries, DEFAULT_FILTERS, SHOW_EVERYTHING } from '@smalltalk/st3-views';
import { simplify } from '@smalltalk/st3-views/conversationSimple';

// Every harness transcript in fixtures/clients/transcripts reads the same in this model as in
// st3-conversation-ui. Rust's model-only tests (crates/st3-conversation-ui/src/tests.rs) check
// the same `*.expected.json`, so the two cannot drift.
const fixture = name => JSON.parse(readFileSync(new URL(`../../../../fixtures/clients/transcripts/${name}`, import.meta.url), 'utf8'));

// The phone's body in stui's shape: `{ kind, value }`, as serde writes stui's `Body`.
function asStui(body) {
  switch (body.kind) {
    case 'mail': {
      const value = { body: body.text, delivered: !!body.delivered, dictated: !!body.dictated, from: body.from, subject: body.subject, to: body.to };
      if (body.images?.length) value.images = body.images;
      return { kind: 'mail', value };
    }
    case 'tool': return { kind: 'tool', value: { output: body.output, state: body.state, title: body.title } };
    default: return { kind: body.kind, value: body.text };
  }
}

for (const name of ['claude', 'codex', 'deliveries', 'native-claude-run', 'native-omp-run']) {
  const expected = fixture(`${name}.expected.json`);
  const phone = conversationEntries(fixture(`${name}.json`), new Map()).map(entry => ({ body: asStui(entry.body), id: entry.id }));
  assert.deepEqual(phone, expected, `${name}: the phone reads this transcript differently from stui`);
}

// A native Claude run: one delivery reads once, and the harness's own records (empty reasoning,
// titles, modes, token reminders) do not sit between tool calls, so the calls collate.
{
  const timeline = fixture('native-claude-run.json');
  const shown = conversationEntries(timeline, new Map());
  const text = JSON.stringify(shown);
  assert.equal(shown.filter(entry => entry.body.kind === 'mail').length, 1, 'the delivery reads once');
  assert.ok(!text.includes('total_tokens_reminder') && !text.includes('last-prompt'));
  assert.ok(!shown.some(entry => entry.body.kind === 'assistant' && entry.body.text.trim() === '[reasoning]'));
  assert.ok(text.includes('Check the queue before answering.'), 'shared reasoning stays');
  assert.ok(text.includes('future-kind'), 'a record st does not know stays visible');
  assert.equal(simplify(shown, new Set()).filter(row => row.kind === 'bundle').length, 1);
  const without = DEFAULT_FILTERS.filter(filter => filter !== 'bookkeeping');
  assert.ok(JSON.stringify(conversationEntries(timeline, new Map(), without)).includes('total_tokens_reminder'));
  assert.equal(conversationEntries(timeline, new Map(), SHOW_EVERYTHING).length, timeline.length);
}
