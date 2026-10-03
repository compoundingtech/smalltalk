import assert from 'node:assert/strict';
import { saidRows } from './conversationSearch.ts';

const found = (overrides = {}) => ({
  kind: 'conversation-search', page: { limit: 20, has_more: false }, indexed_at: '2026-10-02T21:41:00Z', host_id: 'host/example',
  incomplete_sources: [], refreshing: false,
  items: [
    { conversation_id: 'session/one', entry_id: 'entry/7', agent_id: 'agent/example/harbor/keeper', timestamp: '2026-10-02T21:40:00Z', entry_type: 'content', excerpt: 'the harbor keys\nrotated at noon' },
    { conversation_id: 'session/two', entry_id: 'entry/2', timestamp: '2026-10-02T20:00:00Z', entry_type: 'content', excerpt: 'no agent here' },
  ],
  ...overrides,
});
const agents = [{ id: 'agent/example/harbor/keeper', kind: 'agent', name: 'example/harbor/keeper' }];

// A hit in an agent's conversation is a row; one without an agent is left out.
const { rows, note } = saidRows(found(), agents);
assert.deepEqual(rows.map(row => [row.agentId, row.excerpt, row.when]), [['agent/example/harbor/keeper', 'the harbor keys rotated at noon', '10-02 21:40']]);
assert.equal(note, '');
// st's freshness is said.
assert.equal(saidRows(found({ refreshing: true }), agents).note, 'still indexing, more may come');
assert.equal(saidRows(found({ incomplete_sources: ['session/three'] }), agents).note, 'some conversations could not be searched');
