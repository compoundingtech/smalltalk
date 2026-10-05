import assert from 'node:assert/strict';
import { inline, markdown } from './markdown.ts';

assert.deepEqual(inline('a **b** `c` [d](http://e) f'), [
  { text: 'a ', style: 'plain' }, { text: 'b', style: 'bold' }, { text: ' ', style: 'plain' }, { text: 'c', style: 'code' },
  { text: ' ', style: 'plain' }, { text: 'd', style: 'link', url: 'http://e' }, { text: ' f', style: 'plain' },
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

// A written-out address is a link: it ends where the sentence around it does, a closing bracket
// only goes with it when it opened one, and a query or fragment stays.
const link = (url) => ({ text: url, style: 'link', url });
assert.deepEqual(inline('see https://example.com/a/b.'), [{ text: 'see ', style: 'plain' }, link('https://example.com/a/b'), { text: '.', style: 'plain' }]);
assert.deepEqual(inline('(https://example.com/x)'), [{ text: '(', style: 'plain' }, link('https://example.com/x'), { text: ')', style: 'plain' }]);
assert.deepEqual(inline('https://en.wikipedia.org/wiki/Foo_(bar)'), [link('https://en.wikipedia.org/wiki/Foo_(bar)')]);
assert.deepEqual(inline('https://example.com/p?q=1&r=2#frag, then'), [link('https://example.com/p?q=1&r=2#frag'), { text: ', then', style: 'plain' }]);
assert.deepEqual(inline('**https://example.com/b**'), [{ text: 'https://example.com/b', style: 'bold' }]);
// Not a link: a scheme with nothing after it, or one glued to a word.
assert.deepEqual(inline('https:// alone'), [{ text: 'https:// alone', style: 'plain' }]);
assert.deepEqual(inline('nothttps://example.com'), [{ text: 'nothttps://example.com', style: 'plain' }]);
// A markdown link keeps its target; one that is not http(s) is not opened.
assert.deepEqual(inline('[x](javascript:alert)'), [{ text: 'x', style: 'link', url: undefined }]);
