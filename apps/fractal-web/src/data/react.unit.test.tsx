import {
  Agent,
  decodeUnknownSync,
  type AgentEncoded,
  type ResourceObservationEncoded,
} from '@smalltalk/st3-client/schema'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it, vi } from 'vitest'

import { fixtureAttention, fixtureMissions } from '../missions/fixtures.ts'
import { gatewayResources } from '../resources/agent/source.ts'
import { fixtureSource, type FixtureProjections } from './fixtureSource.ts'
import { DataSourceProvider, type SubjectIndex, useSubjectIndex, useSubjectList } from './react.tsx'
import { type DataSource, observed, unavailable, waiting } from './source.ts'

const blocked = unavailable({ reason: 'unsupported', detail: 'This family is not served.' })
const agent = decodeUnknownSync(
  Agent,
  'strict',
)({
  kind: 'agent',
  id: 'agent/discovery',
  name: 'Discovery',
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
const mission = fixtureMissions[0]!
const attention = fixtureAttention[0]!
const world: FixtureProjections = {
  now: 0,
  events: [],
  agents: [agent],
  missions: [mission],
  attention: [attention],
  conversations: {},
  terminals: {},
  envelopes: {},
  usage: { _tag: 'undeclared' },
}

const renderSubjects = (source: DataSource, registry: AtomRegistry.AtomRegistry): string => {
  const Consumer = () => (
    <ul>
      {useSubjectList().map((subject) => (
        <li key={subject.ref}>{subject.ref}</li>
      ))}
    </ul>
  )
  return renderToStaticMarkup(
    <DataSourceProvider source={source} registry={registry}>
      <Consumer />
    </DataSourceProvider>,
  )
}

const readIndex = (source: DataSource, registry: AtomRegistry.AtomRegistry): SubjectIndex => {
  let index: SubjectIndex | undefined
  const Consumer = () => {
    index = useSubjectIndex()
    return null
  }
  renderToStaticMarkup(
    <DataSourceProvider source={source} registry={registry}>
      <Consumer />
    </DataSourceProvider>,
  )
  return index!
}

describe('independent subject discovery through the data provider', () => {
  it.each([waiting, blocked])(
    'keeps observed agents and resources when missions and attention are $_tag',
    async (missing) => {
      const registry = AtomRegistry.make()
      const resource = {
        id: 'resource/example/discovery',
        kind: 'filesystem.file',
        facts: { path: 'flakes/webfractal/src/data/react.tsx' },
        observed_at: '2026-10-04T12:00:00.000Z',
        opened_by: agent.id,
        opened_by_run: null,
      } satisfies ResourceObservationEncoded
      const resources = gatewayResources({
        baseUrl: 'http://fixture.invalid',
        fetchImpl: async (input) =>
          Response.json({
            api_version: 'st3.client.v0',
            snapshot: {},
            value: String(input).endsWith('/v1/client/capabilities')
              ? { limits: { max_page_items: 100 } }
              : {
                  collection: 'resources',
                  kind: 'page',
                  filters: {},
                  items: [resource],
                  page: { has_more: false, next_cursor: null, limit: 50 },
                },
          }),
      })
      const source = {
        ...fixtureSource({ world, overrides: { missions: missing, attention: missing } }),
        resources,
      }
      const unmount = registry.mount(resources.byId(resource.id))
      try {
        await vi.waitFor(() =>
          expect(registry.get(resources.byId(resource.id))._tag).toBe('Observed'),
        )
        const markup = renderSubjects(source, registry)
        expect(markup).toContain(`<li>${agent.id}</li>`)
        expect(markup).toContain(`<li>${resource.id}</li>`)
        expect(markup).not.toContain(`<li>${mission.id}</li>`)
      } finally {
        unmount()
        registry.dispose()
      }
    },
  )

  it.each([
    { ref: mission.id, overrides: { agents: blocked, attention: waiting } },
    { ref: attention.id, overrides: { agents: blocked, missions: waiting } },
  ])('discovers $ref independently of unavailable siblings', ({ ref, overrides }) => {
    const registry = AtomRegistry.make()
    try {
      const source = fixtureSource({ world, overrides })
      expect(renderSubjects(source, registry)).toContain(`<li>${ref}</li>`)
    } finally {
      registry.dispose()
    }
  })

  it('does not delete stale agents when another family becomes unavailable or returns empty', () => {
    const registry = AtomRegistry.make()
    const missions = Atom.make(observed({ value: world.missions })).pipe(Atom.keepAlive)
    const attentionFeed = Atom.make(observed({ value: world.attention })).pipe(Atom.keepAlive)
    const source = {
      ...fixtureSource({
        world,
        overrides: { agents: observed({ value: world.agents, freshness: 'stale' }) },
      }),
      missions,
      attention: attentionFeed,
    }
    try {
      const cached = readIndex(source, registry)
      const terminal = cached.subjects.find((subject) => subject.ref === 'terminal/discovery')
      expect(terminal).toMatchObject({ icon: 'terminal', detail: expect.stringContaining('stale') })
      expect(terminal?.status).toBeUndefined()
      expect(cached.subjects.find((subject) => subject.ref === agent.id)?.detail).toContain('stale')
      expect(
        cached.subjects
          .filter((subject) => subject.ref === attention.id)
          .map((subject) => subject.ref),
      ).toEqual([attention.id])
      expect(renderSubjects(source, registry)).toContain(`<li>${agent.id}</li>`)
      registry.set(missions, blocked)
      registry.set(attentionFeed, waiting)
      expect(renderSubjects(source, registry)).toContain(`<li>${agent.id}</li>`)
      const missing = readIndex(source, registry)
      expect(missing.families.missions._tag).toBe('Unavailable')
      expect(missing.families.attention._tag).toBe('Waiting')
      expect(missing.families.agents._tag === 'Observed' && missing.families.agents.freshness).toBe(
        'stale',
      )
      expect(missing.resources).toBeUndefined()
      registry.set(missions, observed({ value: [] }))
      registry.set(attentionFeed, blocked)
      expect(renderSubjects(source, registry)).toContain(`<li>${agent.id}</li>`)
      const empty = readIndex(source, registry)
      expect(empty.families.missions._tag === 'Observed' && empty.families.missions.value).toEqual(
        [],
      )
      expect(empty.families.attention._tag).toBe('Unavailable')
      registry.set(missions, observed({ value: world.missions, freshness: 'stale' }))
      expect(renderSubjects(source, registry)).toContain(`<li>${mission.id}</li>`)
      expect(renderSubjects(source, registry)).toContain(`<li>${agent.id}</li>`)
    } finally {
      registry.dispose()
    }
  })
})
