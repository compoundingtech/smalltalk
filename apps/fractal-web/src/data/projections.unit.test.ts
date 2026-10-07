import { Agent, Revision, decodeUnknownSync, type AgentEncoded } from '@smalltalk/st3-client/schema'
import { expect, test } from 'vitest'

import { fixtureAttention, fixtureMissions } from '../missions/fixtures.ts'
import { createProjections, fleetFromAgents } from './projections.ts'

test.each([
  { state: 'running', harness_state: 'idle', blocked_on: null, fault: null, activity: 'idle' },
  {
    state: 'running',
    harness_state: 'idle',
    blocked_on: 'human',
    fault: null,
    activity: 'waiting',
  },
  {
    state: 'failed',
    harness_state: 'idle',
    blocked_on: null,
    fault: 'runtime exited',
    activity: 'errored',
  },
] as const)(
  'observed harness activity respects human blocking and failure ($activity)',
  ({ activity, ...observation }) => {
    const row = decodeUnknownSync(
      Agent,
      'strict',
    )({
      kind: 'agent',
      id: 'agent/folder-row',
      name: 'Folder row',
      host_id: 'host/example',
      runtime_ids: [],
      reachability: 'reachable',
      revision: '1',
      updated_at: '2026-10-04T12:00:00.000Z',
      ...observation,
    } satisfies AgentEncoded)
    expect(fleetFromAgents([row]).agents[0]?.activity).toBe(activity)
  },
)

const row = decodeUnknownSync(
  Agent,
  'strict',
)({
  kind: 'agent',
  id: 'agent/first',
  name: 'First',
  host_id: 'host/example',
  runtime_ids: [],
  reachability: 'reachable',
  state: 'running',
  harness_state: 'idle',
  blocked_on: null,
  fault: null,
  revision: '1',
  updated_at: '2026-10-04T12:00:00.000Z',
} satisfies AgentEncoded)

test('incremental fleet projection retains unchanged rows, hosts and collection identity', () => {
  const project = createProjections()
  const second: Agent = { ...row, id: 'agent/second', name: 'Second' }
  const first = project.fleetFromAgents([row, second])
  expect(project.fleetFromAgents([row, second])).toBe(first)
  expect(
    project.fleetFromAgents([
      { ...row, revision: decodeUnknownSync(Revision)('2') },
      { ...second, revision: decodeUnknownSync(Revision)('2') },
    ]),
  ).toBe(first)
  const changed = project.fleetFromAgents([row, { ...second, name: 'Renamed' }])
  expect(changed).not.toBe(first)
  expect(changed.agents[0]).toBe(first.agents[0])
  expect(changed.agents[1]).not.toBe(first.agents[1])
  expect(changed.hosts).toBe(first.hosts)
  const reordered = project.fleetFromAgents([second, row])
  expect(reordered.agents[1]).toBe(first.agents[0])
  expect(reordered.agents[0]).toBe(first.agents[1])
  expect(project.fleetFromAgents([]).agents).toEqual([])
})

test('mission joins retain unrelated mission views and update only linked attention', () => {
  const project = createProjections()
  const mission = fixtureMissions[0]!
  const second: typeof mission = { ...mission, id: 'mission/second' }
  const card = { ...fixtureAttention[0]!, mission_id: mission.id, state: 'open' as const }
  const first = project.missionViews({ missions: [mission, second], attention: [card] })
  expect(project.missionViews({ missions: [mission, second], attention: [card] })).toBe(first)
  const changed = project.missionViews({
    missions: [mission, second],
    attention: [{ ...card, title: 'Changed request' }],
  })
  expect(changed[0]).not.toBe(first[0])
  expect(changed[1]).toBe(first[1])
  const closed = project.missionViews({ missions: [mission, second], attention: [] })
  expect(closed[0]?.attention).toEqual([])
  expect(closed[1]).toBe(first[1])
})

test('subject identities follow only their row and attention membership', () => {
  const project = createProjections()
  const second: Agent = { ...row, id: 'agent/second', name: 'Second' }
  const fleet = project.fleetFromAgents([row, second])
  const first = project.subjectList({ fleet, attention: [] })
  expect(project.subjectList({ fleet, attention: [] })).toBe(first)
  const changed = project.subjectList({
    fleet: project.fleetFromAgents([row, { ...second, name: 'Renamed' }]),
    attention: [],
  })
  expect(changed[0]).toBe(first[0])
  expect(changed[1]).toBe(first[1])
  expect(changed[2]).not.toBe(first[2])
  const card = { ...fixtureAttention[0]!, source_id: row.id, state: 'open' as const }
  const attentive = project.subjectList({ fleet, attention: [card] })
  expect(attentive[0]).toMatchObject({ attention: true })
  expect(attentive[1]).toBe(first[1])
  expect(attentive[2]).toBe(first[2])
  const cleared = project.subjectList({ fleet, attention: [] })
  expect(cleared[0]?.attention).toBeUndefined()
  expect(cleared[2]).toBe(first[2])
})
