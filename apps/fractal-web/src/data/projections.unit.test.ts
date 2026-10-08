import { Agent, AgentId, Revision, decodeUnknownSync, type AgentEncoded } from '@smalltalk/st3-client/schema'
import { expect, test } from 'vitest'

import { fixtureAttention, fixtureMissions } from '../missions/fixtures.ts'
import { createProjections, fleetFromAgents, terminalSubjectForAgent } from './projections.ts'

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

test.each([
  ['agent/example', 'terminal/example'],
  ['agent/example-seat', 'terminal/example-seat'],
  ['agent/team/seat', 'terminal/team/seat'],
])('projects native agent identity %s through the shared terminal subject contract', (id, terminal) => {
  const nativeId = decodeUnknownSync(AgentId)(id)
  expect(terminalSubjectForAgent(nativeId)).toBe(terminal)
  expect(fleetFromAgents([{ ...row, id: nativeId }]).agents[0]?.terminal).toBe(terminalSubjectForAgent(nativeId))
})

test.each(['example-seat', 'team/seat', 'terminal/example', '', 'agent/', 'agent/example seat'])(
  'does not invent a terminal subject for the rejected native identity %s',
  id => {
    expect(() => decodeUnknownSync(AgentId)(id)).toThrow()
    expect(terminalSubjectForAgent(id)).toBeUndefined()
  },
)

test.each([
  { name: 'Declared standing', declaration: { lifecycle: 'standing' }, expected: 'Standing' },
  { name: 'Declared owner', declaration: { lifecycle: 'owner' }, expected: 'Owner' },
  { name: 'Declared bounded', declaration: { lifecycle: 'bounded' }, expected: 'Bounded' },
  { name: 'Absent declaration', declaration: {}, expected: 'Unknown' },
  { name: 'Standing maintenance agent', declaration: {}, expected: 'Unknown' },
] as const)('roster lifecycle decodes $name as $expected', ({ name, declaration, expected }) => {
  const decoded = decodeUnknownSync(Agent, 'strict')({
    kind: 'agent',
    id: 'agent/lifecycle',
    name,
    runtime_ids: [],
    reachability: 'reachable',
    state: 'running',
    revision: '1',
    updated_at: '2026-10-04T12:00:00.000Z',
    ...declaration,
  } satisfies AgentEncoded)
  expect(fleetFromAgents([decoded]).agents[0]).toHaveProperty('lifecycle', { _tag: expected })
})

test('roster lifecycle rejects values outside the generated literal set', () => {
  expect(() => decodeUnknownSync(Agent, 'strict')({
    kind: 'agent', id: 'agent/lifecycle', name: 'Unknown declaration', runtime_ids: [],
    reachability: 'reachable', state: 'running', revision: '1',
    updated_at: '2026-10-04T12:00:00.000Z', lifecycle: 'permanent',
  })).toThrow()
})

test('incremental roster lifecycle changes invalidate the retained sidebar row', () => {
  const project = createProjections()
  const first = project.fleetFromAgents([row])
  const standing = { ...row, lifecycle: 'standing' as const }
  const changed = project.fleetFromAgents([standing])
  expect(changed).not.toBe(first)
  expect(changed.agents[0]).toHaveProperty('lifecycle', { _tag: 'Standing' })
  expect(project.fleetFromAgents([{ ...standing }])).toBe(changed)
  expect(project.fleetFromAgents([{ ...row, lifecycle: 'owner' }]).agents[0])
    .toHaveProperty('lifecycle', { _tag: 'Owner' })
  expect(project.fleetFromAgents([row]).agents[0])
    .toHaveProperty('lifecycle', { _tag: 'Unknown' })
})
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

test('agent roster preserves usage scope, requested checkout, workspace and human attention facts', () => {
  const usage = {
    total_tokens: 123, input_tokens: 80, output_tokens: 43, cached_tokens: 20,
    cache_write_tokens: 5, cost: 0.12, currency: 'USD', incarnation_count: 2,
    aggregation: 'cumulative-per-incarnation-else-response-deltas' as const,
  }
  const observed = decodeUnknownSync(Agent, 'strict')({
    kind: 'agent', id: 'agent/facts', name: 'Facts', runtime_ids: [],
    reachability: 'reachable', state: 'running', revision: '1',
    updated_at: '2026-10-04T12:00:00.000Z',
    usage, checkout: { repository: 'owner/repo', base: 'main', branch: 'feature' },
    workspace: '/work/feature', blocked_on: 'human', ask: 'permission',
    last_activity_at: '2026-10-04T11:59:00.000Z',
    since: '2026-10-04T11:00:00.000Z',
  } satisfies AgentEncoded)
  expect(fleetFromAgents([observed]).agents[0]).toMatchObject({
    usage: { _tag: 'Known', value: usage },
    checkout: { _tag: 'Known', value: { repository: 'owner/repo', base: 'main', branch: 'feature' } },
    workspace: { _tag: 'Known', value: '/work/feature' },
    blockedOn: { _tag: 'Known', value: 'human' }, ask: { _tag: 'Known', value: 'permission' },
    lastActivityAt: { _tag: 'Known', value: Date.parse('2026-10-04T11:59:00.000Z') },
    startedAt: { _tag: 'Unknown' }, endedAt: { _tag: 'Unknown' },
  })
})

test('omitted st roster facts remain Unknown, including session timestamps', () => {
  expect(fleetFromAgents([row]).agents[0]).toMatchObject({
    usage: { _tag: 'Unknown' }, checkout: { _tag: 'Unknown' }, workspace: { _tag: 'Unknown' },
    blockedOn: { _tag: 'Unknown' }, ask: { _tag: 'Unknown' },
    startedAt: { _tag: 'Unknown' }, endedAt: { _tag: 'Unknown' }, lastActivityAt: { _tag: 'Unknown' },
  })
})

test('usage without cost remains known token evidence without fabricated spend', () => {
  const observed = decodeUnknownSync(Agent, 'strict')({
    kind: 'agent', id: 'agent/tokens', name: 'Tokens', runtime_ids: [],
    reachability: 'reachable', state: 'running', revision: '1', updated_at: '2026-10-04T12:00:00.000Z',
    usage: { total_tokens: 0, input_tokens: 0, output_tokens: 0, cached_tokens: 0,
      incarnation_count: 1, aggregation: 'cumulative-per-incarnation-else-response-deltas' },
  } satisfies AgentEncoded)
  const projected = fleetFromAgents([observed]).agents[0]!
  expect(projected.usage._tag).toBe('Known')
  if (projected.usage._tag === 'Known') {
    expect(projected.usage.value.total_tokens).toBe(0)
    expect(projected.usage.value.cost).toBeUndefined()
    expect(projected.usage.value.currency).toBeUndefined()
  }
})

test('roster fact changes update the row while structurally equal usage and checkout retain identity', () => {
  const usage = { total_tokens: 10, input_tokens: 8, output_tokens: 2, cached_tokens: 0,
    incarnation_count: 1, aggregation: 'cumulative-per-incarnation-else-response-deltas' as const }
  const wire = {
    kind: 'agent' as const, id: 'agent/facts', name: 'Facts', runtime_ids: [],
    reachability: 'reachable' as const, state: 'running' as const, revision: '1',
    updated_at: '2026-10-04T12:00:00.000Z',
    usage, checkout: { repository: 'owner/repo', base: 'main', branch: 'feature' },
  } satisfies AgentEncoded
  const project = createProjections()
  const first = project.fleetFromAgents([decodeUnknownSync(Agent, 'strict')(wire)])
  expect(project.fleetFromAgents([decodeUnknownSync(Agent, 'strict')({ ...wire, revision: '2' })])).toBe(first)
  expect(project.fleetFromAgents([decodeUnknownSync(Agent, 'strict')({
    ...wire, usage: { ...usage, total_tokens: 11 },
  })]).agents[0]).not.toBe(first.agents[0])
  expect(project.fleetFromAgents([decodeUnknownSync(Agent, 'strict')({
    ...wire, checkout: { ...wire.checkout, branch: 'changed' },
  })]).agents[0]?.checkout).toEqual({ _tag: 'Known', value: { ...wire.checkout, branch: 'changed' } })
})
