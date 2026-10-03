import assert from 'node:assert/strict';
import { clientName } from './clientName.ts';

assert.equal(clientName('1.2.0', undefined), 'smalltalk-ios 1.2.0');
assert.equal(clientName('1.2.0', 'a0c135e3'), 'smalltalk-ios 1.2.0 (a0c135e3)');
