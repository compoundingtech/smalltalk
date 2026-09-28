import assert from 'node:assert/strict';
import { terminalColor, terminalRunStyle } from './terminalStyle.ts';

const defaults = { fg: '#d6dee3', bg: '#101923' };
assert.equal(terminalColor('#5fd75f'), '#5fd75f');
assert.equal(terminalColor(2), '#98c379');
assert.equal(terminalColor(16), '#000000');
assert.equal(terminalColor(196), '#ff0000');
assert.equal(terminalColor(231), '#ffffff');
assert.equal(terminalColor(232), '#080808');
assert.equal(terminalColor(255), '#eeeeee');

assert.deepEqual(terminalRunStyle({ text: 'plain' }, defaults), { color: '#d6dee3' });
assert.deepEqual(terminalRunStyle({ text: 'x', fg: 1, bold: true, underline: true }, defaults), { color: '#e06c75', fontWeight: 'bold', textDecorationLine: 'underline' });
assert.deepEqual(terminalRunStyle({ text: 'x', inverse: true }, defaults), { color: '#101923', backgroundColor: '#d6dee3' });
assert.deepEqual(terminalRunStyle({ text: 'x', fg: '#010203', bg: 236, inverse: true, dim: true, italic: true }, defaults), { color: '#303030', backgroundColor: '#010203', fontStyle: 'italic', opacity: 0.6 });
