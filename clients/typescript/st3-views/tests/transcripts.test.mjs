import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { conversationEntries, headerLine } from '@smalltalk/st3-views';

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

for (const name of ['claude', 'codex', 'deliveries']) {
  const expected = fixture(`${name}.expected.json`);
  const phone = conversationEntries(fixture(`${name}.json`), new Map()).map(entry => ({ body: asStui(entry.body), id: entry.id }));
  assert.deepEqual(phone, expected, `${name}: the phone reads this transcript differently from stui`);
}

// The OMP parity contract (typed views on #1574 blocks) renders to the same rows as Rust's
// renderer; its timeline arrives as a page object holding the conversation header too.
{
  const expected = fixture('omp-parity.expected.json');
  const page = fixture('omp-parity.json');
  const phone = conversationEntries(page.items, new Map()).map(entry => ({ body: asStui(entry.body), id: entry.id }));
  assert.deepEqual(phone, expected, 'omp-parity: the phone reads this transcript differently from stui');
  assert.equal(headerLine(page.header, '2026-10-06T12:00:00Z'), 'model synthetic/model · context 50 tokens · cost $0.02 · todo 1/5 · jobs 1 · agents 1 · ask Continue? · working [register · 0s ago] · transcript · 0s ago', 'omp-parity: compact header matches stui');
}
