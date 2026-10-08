import assert from 'node:assert/strict';
import { documentText, documentTitle, isDocumentName } from './documents.ts';

assert.equal(isDocumentName('doc/fleet/stui/resource-sidebar-proposal@32f20950d2335a4816cc7bbfe80a5594bba3e4a6c148afcf91f2499ed02399e0'), true);
assert.equal(isDocumentName('doc/fleet/stui/notes'), false, 'a name without its hash is not one document');
assert.equal(isDocumentName('https://example.com/doc'), false);
assert.equal(isDocumentName(undefined), false);
assert.equal(documentTitle('doc/fleet/stui/resource-sidebar-proposal@32f2'), 'resource-sidebar-proposal');
assert.equal(documentText([0x23, 0x20, 0xc3, 0xa9]), '# é');
