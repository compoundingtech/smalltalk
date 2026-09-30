import assert from 'node:assert/strict';
import { inline, markdown } from './markdown.ts';

assert.deepEqual(inline('a **b** `c` [d](http://e) f'), [
  { text: 'a ', style: 'plain' }, { text: 'b', style: 'bold' }, { text: ' ', style: 'plain' }, { text: 'c', style: 'code' },
  { text: ' ', style: 'plain' }, { text: 'd', style: 'link' }, { text: ' f', style: 'plain' },
]);
// An unclosed marker is plain text.
assert.deepEqual(inline('2 ** 3'), [{ text: '2 ** 3', style: 'plain' }]);

const blocks = markdown('\n# Title\n\n- one\n  2. two\n> said\n---\n```rust\nfn main() {}\n```\n| a | b |\n|---|---|\n| 1 | 2 |\nplain\n\n');
assert.deepEqual(blocks.map(block => block.kind), ['heading', 'blank', 'item', 'item', 'quote', 'rule', 'fence', 'code', 'fence', 'table', 'table', 'text']);
assert.equal(blocks[3].indent, 2);
assert.equal(blocks[3].marker, '2. ');
assert.equal(blocks[6].text, '```rust');
// A fence that never closes still ends the code.
assert.deepEqual(markdown('```\nlet x').map(block => block.kind), ['fence', 'code', 'fence']);
