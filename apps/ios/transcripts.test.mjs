import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { conversationEntries } from './conversationView.ts';

// Every harness transcript in fixtures/clients/transcripts reads the same on the phone as in stui:
// stui writes each `*.expected.json` from its own parser (crates/stui/src/ui/contract.rs), and
// the phone's parser must arrive at the same entries, so the two cannot drift.
const fixture = name => JSON.parse(readFileSync(new URL(`../../fixtures/clients/transcripts/${name}`, import.meta.url), 'utf8'));

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
