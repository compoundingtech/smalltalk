import assert from 'node:assert/strict';
import { openSubagentConversation } from './conversationNavigation.ts';

// Record the native-stack boundary without loading React Native in the Node unit runner.
const parent = { name: 'Conversation', params: { target: 'session/parent', sessionId: 'session/parent', title: 'Parent' } };
const routes = [{ name: 'AgentsRoot' }, parent];
const navigation = { push: (name, params) => { routes.push({ name, params }); } };
openSubagentConversation(navigation, 'session/child', 'ParityChild');
assert.deepEqual(routes, [{ name: 'AgentsRoot' }, parent, {
  name: 'Conversation', params: { target: 'session/child', sessionId: 'session/child', title: 'ParityChild' },
}]);
// Native Back removes the pushed route, preserving the exact parent, not the Agents root.
routes.pop();
assert.equal(routes.at(-1), parent);
