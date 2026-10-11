import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { COLLAPSED_TOOL_LINES, folds, HOME_LEGEND } from '@smalltalk/st3-views';
import { tokenColor } from './conversationStyle.ts';

// stui writes the drawing rules; the phone draws from the same file. Every colour it names must
// be one of the phone's theme tokens, and the phone's fold must match stui's.
const rules = JSON.parse(readFileSync(new URL('../../fixtures/clients/conversation-style.json', import.meta.url), 'utf8'));
const tokens = [];
const walk = (value, key) => {
  if (typeof value === 'string' && /^(fill|edge|from|text|rows|title|color|time|rule|label|running|added|removed|failed|unconfirmed|sending_edge|to_you_fill|assistant)$/.test(key) && !/[^a-z0-9_]/.test(value)) tokens.push(value);
  else if (value && typeof value === 'object') for (const [name, inner] of Object.entries(value)) walk(inner, name);
};
walk(rules, '');
assert.ok(tokens.length > 20);
for (const token of tokens) assert.match(tokenColor(token), /^#[0-9a-f]{6}$/, token);
assert.equal(tokenColor('tool_bg'), '#232436');
assert.equal(rules.tool.collapsed_rows, COLLAPSED_TOOL_LINES);

assert.equal(folds({ kind: 'tool', title: 'x', state: 'ok', output: [] }), true);
assert.equal(folds({ kind: 'mail', from: 'planner', to: 'builder', subject: '', text: '' }), true);
assert.equal(folds({ kind: 'mail', from: 'planner', to: 'you', subject: '', text: '' }), false);
assert.equal(folds({ kind: 'mail', from: 'you', to: 'planner', subject: '', text: '' }), false);

// Home retains the phone's decision and information colors after model extraction.
assert.deepEqual(HOME_LEGEND.map(entry => tokenColor(entry.color)), ['#cba6f7', '#a6e3a1', '#cba6f7']);
assert.equal(HOME_LEGEND.at(-1).word, 'breached');
assert.equal(HOME_LEGEND.at(-1).glyph, '!');
