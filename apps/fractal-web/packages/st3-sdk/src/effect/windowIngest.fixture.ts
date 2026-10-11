import type { Agent, CollectionFrame, Resource, Snapshot } from '@smalltalk/st3-client'

export const snapshot: Snapshot = {
  id: 'snapshot/roster-perf',
  created_at: '2026-10-06T00:00:00Z',
  host_id: 'host/build-a',
  projection_version: 'client-projection.v0',
  store_index: 1,
}
export const agents: Agent[] = Array.from({ length: 200 }, (_, i) => ({
  id: `agent/perf-${i}`,
  kind: 'agent',
  revision: '1',
  updated_at: snapshot.created_at,
  name: `Agent ${i}`,
  state: 'running',
  reachability: 'local',
  runtime_ids: [],
  description: 'Realistic retained roster description. '.repeat(16),
}))
export const frame = (items: Resource[]): CollectionFrame => ({
  kind: 'snapshot',
  id: 'f1',
  collection: 'agents',
  items,
  order: items.map((row) => row.id),
  has_more: false,
  snapshot,
})
