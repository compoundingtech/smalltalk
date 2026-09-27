import assert from 'node:assert/strict';
import { agentLabel, agentTree } from './agentTree.ts';

const parent = { id: 'agent/parent', name: 'Parent', under: [] };
const child = { id: 'agent/child', name: 'Child', under: [{ agent_id: 'agent/parent' }] };
const grandchild = { id: 'agent/grandchild', name: 'Grandchild', under: [{ agent_id: 'agent/child' }] };
assert.deepEqual(agentTree([grandchild, child, parent]).map(row => [row.agent.name, row.depth]), [['Parent', 0], ['Child', 1], ['Grandchild', 2]]);
assert.deepEqual(agentTree([{ id: 'agent/a', under: [{ agent_id: 'agent/b' }] }, { id: 'agent/b', under: [{ agent_id: 'agent/a' }] }]).map(row => row.agent.id), ['agent/a', 'agent/b']);
assert.deepEqual(agentTree(undefined), []);
assert.equal(agentLabel({ id: 'agent/fleet/st3/standing/st3', name: 'fleet/st3/standing/st3' }), 'ST3');
