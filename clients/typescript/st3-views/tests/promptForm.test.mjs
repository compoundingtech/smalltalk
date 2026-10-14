import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { offeredPromptAnswers, promptForm } from '@smalltalk/st3-views/promptForm';

// The cases web, tui and ios share; crates/st3-ui-model checks the same file in Rust.
const cases = JSON.parse(readFileSync(new URL('./fixtures/prompt-forms.json', import.meta.url), 'utf8'));
assert.ok(cases.length >= 8);
for (const each of cases) {
  const form = promptForm(each.harness, each.answers, each.prompt, each.calls);
  assert.equal(form.question, each.form.question, each.name);
  assert.equal(form.mark, each.form.mark, each.name);
  assert.equal(form.elsewhere, each.form.elsewhere, each.name);
  assert.deepEqual(form.options.map(({ answer, label, enter }) => ({ answer, label, enter })), each.form.options, each.name);
}

// Allow only beside one call shown; never beside two, or a call with nothing to show.
const ls = { tool: 'Bash', input: { command: 'ls' } };
assert.deepEqual(offeredPromptAnswers(['allow', 'deny'], [ls]), ['allow', 'deny']);
assert.deepEqual(offeredPromptAnswers(['allow', 'deny'], [ls, ls]), ['deny']);
assert.deepEqual(offeredPromptAnswers(['allow', 'deny'], [{ tool: 'Bash', input: {} }]), ['deny']);
assert.deepEqual(offeredPromptAnswers(['answer', 'deny'], [ls]), ['deny']);
