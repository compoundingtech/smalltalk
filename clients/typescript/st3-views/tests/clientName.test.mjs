import assert from 'node:assert/strict';
import { clientName } from '@smalltalk/st3-views/clientName';

assert.equal(clientName('smalltalk-ios', '1.2.0', undefined), 'smalltalk-ios 1.2.0');
assert.equal(clientName('smalltalk-ios', '1.2.0', 'a0c135e3'), 'smalltalk-ios 1.2.0 (a0c135e3)');

assert.equal(clientName('smalltalk-ide', '2.0.0'), 'smalltalk-ide 2.0.0');
