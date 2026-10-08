// @vitest-environment jsdom
import {
  Agent,
  Attention,
  Revision,
  decodeUnknownSync,
  type AgentEncoded,
} from '@smalltalk/st3-client/schema'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import * as React from 'react'
import { flushSync } from 'react-dom'
import { createRoot } from 'react-dom/client'
import { expect, it, vi } from 'vitest'
// Node tests stub only the CSS runtime; render identity and feed equality are under test.
vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}),
  keyframes: () => 'test-animation',
  props: () => ({}),
}))

import { sameFolderFleet } from '../shell/AgentFolders.tsx'
import { useAgentAttentionCounts } from '../shell/AgentAttention.tsx'
import { WorkbenchContextProvider, useOpen, type SubjectSummary } from '../shell/context.tsx'
import { defaultFilters } from '../shell/sidebar/state.ts'
import { fixtureSource, type FixtureProjections } from './fixtureSource.ts'
import { createProjections } from './projections.ts'
import { DataSourceProvider, useAgent, useFleetSelection, useTerminalConnected } from './react.tsx'
import { type Feed, type Fleet, observed } from './source.ts'

const first = decodeUnknownSync(
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
const second: Agent = { ...first, id: 'agent/second', name: 'Second' }
const card = decodeUnknownSync(Attention)({
  kind: 'attention',
  id: 'attention/second',
  source_id: second.id,
  person_id: 'person/operator',
  attention_kind: 'agent-request',
  title: 'Second request',
  detail: 'Independent request',
  priority: 'normal',
  state: 'open',
  actions: [],
  revision: '1',
  requested_at: '2026-10-04T12:00:00.000Z',
  updated_at: '2026-10-04T12:00:00.000Z',
})
const world: FixtureProjections = {
  now: 0,
  events: [],
  agents: [first, second],
  missions: [],
  attention: [],
  conversations: {},
  terminals: {},
  envelopes: {},
  usage: { _tag: 'undeclared' },
}
const identityFleet = (feed: Feed<Fleet>): Feed<Fleet> => feed
const equalManualFleet = (left: Feed<Fleet>, right: Feed<Fleet>) =>
  sameFolderFleet(left, right, defaultFilters)

it('updates only the changed row, not the manual collection, opened agent or terminal', () => {
  const registry = AtomRegistry.make()
  const agents = Atom.make<Feed<readonly Agent[]>>(observed({ value: world.agents })).pipe(
    Atom.keepAlive,
  )
  const attention = Atom.make<Feed<readonly Attention[]>>(observed({ value: [] })).pipe(
    Atom.keepAlive,
  )
  const source = { ...fixtureSource({ world }), agents, attention }
  const renders = { first: 0, second: 0, collection: 0, opened: 0, terminal: 0 }
  const Row = ({ refId }: { readonly refId: 'first' | 'second' }) => {
    renders[refId]++
    const feed = useAgent(`agent/${refId}`)
    const facts = useAgentAttentionCounts(`agent/${refId}`)
    return (
      <p>
        {feed._tag === 'Observed' ? feed.value?.name : feed._tag}:{facts.decisions}
      </p>
    )
  }
  const Collection = () => {
    renders.collection++
    useFleetSelection({ select: identityFleet, equal: equalManualFleet })
    return (
      <>
        <Row refId="first" />
        <Row refId="second" />
      </>
    )
  }
  const Opened = ({ agentRef }: { readonly agentRef: string }) => {
    renders.opened++
    const feed = useAgent(agentRef)
    return <p>{feed._tag === 'Observed' ? feed.value?.name : feed._tag}</p>
  }
  const Terminal = () => {
    renders.terminal++
    return <p>{String(useTerminalConnected('terminal/first'))}</p>
  }
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  const render = (agentRef: string) =>
    flushSync(() =>
      root.render(
        <DataSourceProvider source={source} registry={registry}>
          <Collection />
          <Opened agentRef={agentRef} />
          <Terminal />
        </DataSourceProvider>,
      ),
    )
  try {
    render(first.id)
    const before = { ...renders }
    const changed: Agent = {
      ...second,
      name: 'Renamed second',
      revision: decodeUnknownSync(Revision)('2'),
    }
    flushSync(() => registry.set(agents, observed({ value: [first, changed] })))
    expect(renders).toEqual({ ...before, second: before.second + 1 })
    const afterAgent = { ...renders }
    flushSync(() => registry.set(attention, observed({ value: [card] })))
    expect(renders).toEqual({ ...afterAgent, second: afterAgent.second + 1 })
    const beforeMetadata = { ...renders }
    const freshFirst: Agent = { ...first, revision: decodeUnknownSync(Revision)('3') }
    const freshSecond: Agent = { ...changed, revision: decodeUnknownSync(Revision)('3') }
    flushSync(() => registry.set(agents, observed({ value: [freshFirst, freshSecond] })))
    flushSync(() =>
      registry.set(
        attention,
        observed({ value: [{ ...card, revision: decodeUnknownSync(Revision)('3') }] }),
      ),
    )
    expect(renders).toEqual(beforeMetadata)
    render(second.id)
    const afterSwitch = { ...renders }
    flushSync(() =>
      registry.set(agents, observed({ value: [{ ...first, name: 'Unrelated first' }, changed] })),
    )
    expect(renders.opened).toBe(afterSwitch.opened)
    expect(renders.second).toBe(afterSwitch.second)
    expect(renders.collection).toBe(afterSwitch.collection)
    expect(renders.terminal).toBe(afterSwitch.terminal)
    const beforeStale = { ...renders }
    flushSync(() => registry.set(agents, observed({ value: [first, changed], freshness: 'stale' })))
    expect(renders.first).toBeGreaterThan(beforeStale.first)
    expect(renders.second).toBeGreaterThan(beforeStale.second)
    expect(renders.opened).toBeGreaterThan(beforeStale.opened)
  } finally {
    flushSync(() => root.unmount())
    registry.dispose()
    container.remove()
  }
})

it('subject roster changes do not notify open-action consumers', () => {
  let renders = 0
  const open = () => undefined
  const Consumer = () => {
    renders++
    useOpen()
    return <p>Opened content</p>
  }
  const child = <Consumer />
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  const render = (subjects: ReadonlyMap<string, SubjectSummary>) =>
    flushSync(() =>
      root.render(
        <WorkbenchContextProvider
          value={{ open, subjects, focusedRef: first.id, platform: 'other' }}
        >
          {child}
        </WorkbenchContextProvider>,
      ),
    )
  try {
    render(new Map([[first.id, { ref: first.id, title: first.name, icon: 'conversation' }]]))
    const before = renders
    render(
      new Map([[second.id, { ref: second.id, title: 'Unrelated rename', icon: 'conversation' }]]),
    )
    expect(renders).toBe(before)
  } finally {
    flushSync(() => root.unmount())
    container.remove()
  }
})

it('invalidates folder membership/order only for active filter dependencies', () => {
  const project = createProjections()
  const fleet = project.fleetFromAgents([first, second])
  const left = observed({ value: fleet })
  // The contextual fleet-agent type keeps union tags literal; an untyped literal widens
  // `_tag`/`activity` to string and the feed stops being a Feed<Fleet>.
  const changed: Fleet['agents'][number] = {
    ...fleet.agents[0]!,
    activity: 'waiting',
    description: 'Changed description',
    lastActivityAt: { _tag: 'Known', value: 1 },
  }
  const right = observed({
    value: {
      ...fleet,
      agents: [changed, fleet.agents[1]!],
    },
  })
  expect(sameFolderFleet(left, right, defaultFilters)).toBe(true)
  expect(sameFolderFleet(left, right, { ...defaultFilters, needsMe: true })).toBe(false)
  expect(sameFolderFleet(left, right, { ...defaultFilters, query: 'Changed' })).toBe(false)
  expect(sameFolderFleet(left, right, { ...defaultFilters, sort: 'activity' })).toBe(false)
  expect(sameFolderFleet(left, right, { ...defaultFilters, sort: 'status' })).toBe(false)
  expect(
    sameFolderFleet(left, observed({ value: fleet, freshness: 'stale' }), defaultFilters),
  ).toBe(true)
  expect(
    sameFolderFleet(left, observed({ value: fleet, freshness: 'stale' }), {
      ...defaultFilters,
      statuses: ['idle'],
    }),
  ).toBe(false)
})
