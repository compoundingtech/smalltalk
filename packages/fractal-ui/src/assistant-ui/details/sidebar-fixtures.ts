import type { SidebarAgentRow } from '../sidebar/model'

/** Frozen dt10 sidebar projection, limited to the facts consumed by Details.
 * SDK DTOs, controllers and arrangement state remain outside this presentation package. */
export const fixtureSidebarNow = Date.parse('2026-10-07T12:00:00.000Z')
const unknown = { _tag: 'Unknown' } as const
const children: readonly SidebarAgentRow[] = [
  { ref: 'agent/sample/subagent/sample-child', id: 'sample/subagent/sample-child', parentRef: 'agent/sample', title: 'Check output', description: 'Check output', host: 'local', harness: 'explore', status: 'unobserved', statusLabel: 'Not observed', needsMe: false, freshness: 'unobserved', unread: 0, usage: unknown, duration: unknown, lastTurn: unknown, children: [], subagent: { id: 'sample-child', lease_expires_at: '2026-10-07T12:20:00.000Z', session_id: 'synthetic-child-0-0', started_at: '2026-10-07T11:56:00.000Z', subagent_type: 'explore', work_id: 'step-run/workshop-1' } },
  { ref: 'agent/sample/subagent/worker-2', id: 'sample/subagent/worker-2', parentRef: 'agent/sample', title: 'Review workspace 1 edge cases', description: 'Review workspace 1 edge cases', host: 'local', harness: 'review', status: 'unobserved', statusLabel: 'Not observed', needsMe: false, freshness: 'unobserved', unread: 0, usage: unknown, duration: unknown, lastTurn: unknown, children: [], subagent: { id: 'worker-2', lease_expires_at: '2026-10-07T12:20:00.000Z', session_id: 'synthetic-child-0-1', started_at: '2026-10-07T11:55:00.000Z', subagent_type: 'review', work_id: 'step-run/workshop-1' } },
]
export const fixtureSidebarRows: ReadonlyMap<string, SidebarAgentRow> = new Map([
  ['agent/sample', { ref: 'agent/sample', id: 'sample', title: 'sample count sync', description: 'Reconcile the decoded source rows', host: 'local', harness: 'omp', model: 'reported-model-a', branch: 'workshop/observation-1-with-a-long-descriptive-branch-name', status: 'working', statusLabel: 'Working', needsMe: true, freshness: 'live', usage: unknown, duration: unknown, lastTurn: unknown, children }],
  ['agent/source-archive', { ref: 'agent/source-archive', id: 'source-archive', title: 'fixture regeneration', description: 'Prepare a decision for the operator', host: 'build-west', harness: 'codex', model: 'reported-model-c', status: 'idle', statusLabel: 'Idle', needsMe: false, freshness: 'live', usage: unknown, duration: unknown, lastTurn: unknown, children: [] }],
])
export const fixtureMissionDTOs = [
  { id: 'mission/workshop-1', title: 'Review the source projection', state: 'running' },
  { id: 'mission/workshop-2', title: 'Reconcile the repository', state: 'running' },
  { id: 'mission/workshop-3', title: 'Maintain the public workshop', state: 'standing' },
] as const
export const fixtureAttentionDTOs = [
  { id: 'attention/workshop-1', title: 'Review requested · sample', detail: 'Synthetic decision card; reading this clears its local unread marker.', source_id: 'agent/sample' },
] as const
