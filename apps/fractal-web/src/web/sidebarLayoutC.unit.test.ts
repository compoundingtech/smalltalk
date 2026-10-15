import { describe, expect, it } from 'vitest'
import type { SidebarSpendObservation } from '../extensions/contract.ts'
import { sidebarRow } from './sidebarRow.ts'
import type { Agent } from '../data/source.ts'

const unknown = { _tag: 'Unknown' } as const
const agent: Agent = { ref: 'agent/builder', terminal: '', name: 'builder session', lifecycle: unknown, host: 'local', connected: true, harness: 'omp', incarnationId: 'inc/current', activity: 'working', status: 'working', usage: unknown, checkout: unknown, workspace: unknown, startedAt: unknown, endedAt: unknown, blockedOn: unknown, ask: unknown, lastActivityAt: unknown }
const observation: SidebarSpendObservation = {
  agentRef: agent.ref, incarnationId: 'inc/current', nativeSessionId: '00000000-0000-4000-8000-000000000001',
  spend: { last24h: { _tag: 'Known', usd: 0, since: 0, until: 86_400_000, observedAt: 86_400_000 }, last7d: { _tag: 'Unknown' } },
}
const project = (spendObservation?: SidebarSpendObservation, change = {}) => sidebarRow({ agent: { ...agent, ...change }, stale: false, now: 86_400_000, spendObservation })
describe('Layout C reported row windows', () => {
  it('preserves reported zero and an independently unknown seven-day window', () => {
    expect(project(observation).spend).toEqual(observation.spend)
    expect(project().spend).toBeUndefined()
    expect(() => expect(project().spend).toEqual(observation.spend)).toThrow()
  })
  it('requires the current exact agent/incarnation and native omp harness', () => {
    expect(project({ ...observation, agentRef: 'agent/unrelated' }).spend).toBeUndefined()
    expect(project({ ...observation, incarnationId: 'inc/previous' }).spend).toBeUndefined()
    expect(project(observation, { incarnationId: undefined }).spend).toBeUndefined()
    expect(project(observation, { harness: 'codex' }).spend).toBeUndefined()
  })
})
