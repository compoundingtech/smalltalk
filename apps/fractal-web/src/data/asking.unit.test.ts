import { Agent, Attention, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import { describe, expect, it } from 'vitest'

import { askingAgentRefs } from './asking.ts'

const agent = (id: string, fields: Record<string, unknown> = {}) => decodeUnknownSync(Agent)({
  kind: 'agent', id, name: id, host_id: 'host/host-a', runtime_ids: [], reachability: 'reachable',
  state: 'running', harness_state: 'idle', blocked_on: null, fault: null, revision: '1', updated_at: '2026-10-04T12:00:00.000Z',
  ...fields,
})
const card = (fields: Record<string, unknown>) => decodeUnknownSync(Attention)({
  id: 'attention/card', kind: 'attention', revision: '1', updated_at: '2026-10-04T12:00:00Z', requested_at: '2026-10-04T12:00:00Z',
  person_id: 'person/operator', source_id: 'agent/elsewhere', attention_kind: 'agent-request', title: 'Request', detail: 'Request detail', priority: 'normal',
  state: 'open', actions: [], ...fields,
})
const onMission = (mission: string) => ({
  current_work: [{ id: 'work/one', mission_id: mission, mission_run_id: 'mission-run/one', path: 'step', since: '2026-10-04T12:00:00.000Z', state: 'claimed', title: 'Step', goal: 'Goal' }],
})
const asking = (rows: readonly Agent[], attention: readonly Attention[] = []) => [...askingAgentRefs({ rows, attention })]

describe('Waiting for answer reads only st', () => {
  it.each([
    ['while blocked on a person', { blocked_on: 'human', ask: 'question' }],
    ['from the ask field alone (st sets it only while blocked on a person)', { ask: 'question' }],
  ])('maps an ask-tool question %s', (_, fields) => {
    expect(asking([agent('agent/q', fields)])).toEqual(['agent/q'])
  })

  it.each([
    ['a permission ask', { blocked_on: 'human', ask: 'permission' }],
    ['a review ask', { blocked_on: 'human', ask: 'review' }],
    ['a human block without an ask kind', { blocked_on: 'human' }],
    ['a waiting state alone', { state: 'waiting' }],
  ])('does not map %s', (_, fields) => {
    expect(asking([agent('agent/x', fields)])).toEqual([])
  })

  it('maps open attention raised by, requested by, or on a current mission of the agent', () => {
    const rows = [agent('agent/source'), agent('agent/requester'), agent('agent/mission', onMission('mission/one')), agent('agent/none')]
    expect(asking(rows, [card({ source_id: 'agent/source' })])).toEqual(['agent/source'])
    expect(asking(rows, [card({ requester_id: 'agent/requester' })])).toEqual(['agent/requester'])
    expect(asking(rows, [card({ mission_id: 'mission/one' })])).toEqual(['agent/mission'])
    expect(asking(rows, [card({ mission_id: 'mission/other' })])).toEqual([])
  })

  it('maps open attention owned by or shown in the agent conversation, as st links shared cards', () => {
    const rows = [agent('agent/atlas'), agent('agent/harbor'), agent('agent/owner'), agent('agent/none')]
    // A shared login: raised by atlas, shown in every seat it covers; harbor is linked only by conversation_ids.
    const sharedLogin = { source_id: 'agent/atlas', conversation_id: 'agent/atlas', conversation_ids: ['agent/atlas', 'agent/harbor'] }
    expect(asking(rows, [card(sharedLogin)])).toEqual(['agent/atlas', 'agent/harbor'])
    // st resolves the owning conversation from a run while requester and mission are absent.
    const runOwned = { source_id: 'run/one', conversation_id: 'agent/owner' }
    expect(asking(rows, [card(runOwned)])).toEqual(['agent/owner'])
    expect(asking(rows, [card({ ...sharedLogin, state: 'resolved' }), card({ ...runOwned, state: 'resolved' })])).toEqual([])
  })

  it('ignores attention that is no longer open', () => {
    expect(asking([agent('agent/source')], [card({ source_id: 'agent/source', state: 'resolved' })])).toEqual([])
  })
})
