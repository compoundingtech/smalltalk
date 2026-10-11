import { composeStory } from '@storybook/react'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it } from 'vitest'

import { ANCHOR_MS, catalog, foldSlice, loadWorld, manualClock, syncStatusAt, type SliceKind } from '../src/index.ts'
import { createReadTracker, ScenarioProvider, useScenarioSlice, type WireSlice } from '../src/react/index.ts'
import { scenarioArgTypes, scenarioGlobalTypes, scenarioStoryCheck, ScenarioStoryCheckError, withScenario, type ScenarioStoryCheckOptions } from '../src/storybook/index.ts'
import meta, { Fixed, Good, Invariant, LoadingRoster, Pinned, Sync, Undeclared } from './stories/Consumption.stories.tsx'

const world = loadWorld('fleet-mid-refactor', { now: ANCHOR_MS })

const captureConversation = (clock = manualClock(world.now)): WireSlice<'conversation'> => {
  const observed: { value?: WireSlice<'conversation'> } = {}
  const Probe = () => {
    observed.value = useScenarioSlice('conversation')
    return <p>{observed.value.state.threads.flatMap(({ items }) => items).length}</p>
  }
  renderToStaticMarkup(<ScenarioProvider world={world} clock={clock}><Probe /></ScenarioProvider>)
  if (observed.value === undefined) throw new Error('Conversation probe did not render')
  return observed.value
}

describe('React scenario binding', () => {
  it('hands out wire state and folds conversation events at 2s, 3s, and 5s', () => {
    const clock = manualClock(world.now)
    const initial = captureConversation(clock)
    expect(initial).toEqual({ variant: 'default', loading: false, state: world.slices.conversation.state })
    let previous = initial
    for (const delta of [2_000, 1_000, 2_000]) {
      clock.advance(delta)
      const current = captureConversation(clock)
      expect(current.state).toEqual(foldSlice(world.slices.conversation, clock.now() - world.now).state)
      expect(current.state).not.toEqual(previous.state)
      previous = current
    }
  })

  it('projects wire state and uses a manual clock at world.now by default', () => {
    const Probe = () => <p>{useScenarioSlice('roster', ({ state }) => state.agents.map(({ name }) => name).join(' | '))}</p>
    const markup = renderToStaticMarkup(<ScenarioProvider world={world}><Probe /></ScenarioProvider>)
    for (const agent of world.slices.roster.state.agents) expect(markup).toContain(agent.name)
  })

  it('records exactly the read kinds, including structural replay served reads', () => {
    const served = new Set<SliceKind>(['terminal'])
    const tracker = createReadTracker({ served: () => served })
    const Probe = () => {
      useScenarioSlice('roster')
      useScenarioSlice('roster')
      useScenarioSlice('attention')
      return <p>Read probe</p>
    }
    renderToStaticMarkup(<ScenarioProvider world={world} tracker={tracker}><Probe /></ScenarioProvider>)
    expect(tracker.reads()).toEqual(new Set(['roster', 'attention', 'terminal']))
    served.add('details')
    expect(tracker.reads()).toEqual(new Set(['roster', 'attention', 'terminal', 'details']))
  })

  it('accepts a served source directly and exposes portable sync statuses', () => {
    const observed: { value?: WireSlice<'sync'> } = {}
    const dropped = world.with({ sync: 'socket-dropped' })
    const clock = manualClock(world.now + 4_000)
    const Probe = () => { observed.value = useScenarioSlice('sync'); return <p>Sync probe</p> }
    renderToStaticMarkup(<ScenarioProvider world={dropped} clock={clock} tracker={{ served: () => new Set(['sync']) }}><Probe /></ScenarioProvider>)
    expect(observed.value?.status).toEqual(syncStatusAt(dropped.slices.sync, 4_000, dropped.now))
    expect(observed.value?.status.agents?._tag).toBe('Stale')
  })

  it('rejects hooks outside the provider', () => {
    const Probe = () => { useScenarioSlice('roster'); return <p /> }
    expect(() => renderToStaticMarkup(<Probe />)).toThrow('useScenarioSlice requires ScenarioProvider')
  })
})

describe('Storybook scenario binding', () => {
  it('lists the catalog and offers the world variants as controls', () => {
    expect(scenarioGlobalTypes.scenario.defaultValue).toBe('fleet-mid-refactor')
    expect(scenarioGlobalTypes.scenario.toolbar.items.map(({ value }) => value)).toEqual(catalog.map(({ id }) => id))
    const controls = scenarioArgTypes(['roster', 'attention'], world)
    expect(controls.roster?.options).toEqual(world.available.roster)
    expect(controls.attention?.defaultValue).toBe('default')
  })

  it('uses world pinning before toolbar selection and args before slice defaults', () => {
    const Composed = composeStory(Pinned, meta, {
      decorators: [withScenario],
      globalTypes: scenarioGlobalTypes,
      initialGlobals: { scenario: 'failed-sync-socket-dropped', scenarioNow: ANCHOR_MS },
    })
    const markup = renderToStaticMarkup(<Composed roster="one-agent" />)
    expect(markup).toContain('data-scenario="fleet-mid-refactor"')
    expect(markup).toContain('data-scenario-slices="roster:one-agent,attention:default"')
    expect(markup).toContain(world.slices.roster.state.agents[0]!.name!)
    expect(markup).not.toContain(world.slices.roster.state.agents[1]!.name!)
  })

  it('passes a story that renders roster names and attention titles, including URL state', () => {
    const result = scenarioStoryCheck(Good, meta, { scenarioNow: ANCHOR_MS, assert: true })
    expect(result._tag).toBe('Passed')
    expect(result.failures).toEqual([])
    expect(result.reads).toEqual(new Set(['roster', 'attention']))
    expect(result.checks.find(({ check }) => check === 5)?._tag).toBe('Passed')
    expect(result.checks.find(({ check }) => check === 3)?._tag).toBe('Passed')
  })

  it('reports both missing reads and missing data dependence for fixed text', () => {
    const result = scenarioStoryCheck(Fixed, meta)
    expect(result._tag).toBe('Failed')
    expect(result.failures).toContainEqual(expect.objectContaining({ _tag: 'Reads', check: 1, message: expect.stringContaining('missing [roster]') }))
    expect(result.failures).toContainEqual(expect.objectContaining({ _tag: 'DataDependence', check: 2, slice: 'roster', message: 'different wire states render identical content' }))
    expect(result.failures).toContainEqual(expect.objectContaining({ _tag: 'WorldSwitch', check: 3, slice: 'roster' }))
    expect(() => scenarioStoryCheck(Fixed, meta, { assert: true })).toThrow(ScenarioStoryCheckError)
  })

  it('reports undeclared reads even when declared data renders correctly', () => {
    const result = scenarioStoryCheck(Undeclared, meta)
    expect(result.failures).toContainEqual(expect.objectContaining({ _tag: 'Reads', check: 1, message: expect.stringContaining('undeclared [details]') }))
    expect(result.failures.some(({ check }) => check === 2)).toBe(false)
  })

  it('checks that a pinned world is invariant to toolbar switches', () => {
    const result = scenarioStoryCheck(Pinned, meta)
    expect(result._tag).toBe('Passed')
    expect(result.checks.find(({ check }) => check === 4)?._tag).toBe('Passed')
    expect(result.checks.find(({ check }) => check === 3)?._tag).toBe('Skipped')
  })

  it('tests sync contrasts and world switching at the dropped-socket transition', () => {
    const result = scenarioStoryCheck(Sync, meta)
    expect(result._tag).toBe('Passed')
    expect(result.checks.find(({ check }) => check === 2)?._tag).toBe('Passed')
    expect(result.checks.find(({ check }) => check === 3)?._tag).toBe('Passed')
  })

  it('renders sync causes even when both contrast sides have the same status tag', () => {
    const options = { atMs: 4_000, contrastPairs: { sync: [['socket-dropped', 'open-fail']] } } satisfies ScenarioStoryCheckOptions
    const result = scenarioStoryCheck(Sync, meta, options)
    expect(result._tag).toBe('Passed')
    expect(result.checks.find(({ check }) => check === 2)?._tag).toBe('Passed')
    const TagsOnly = () => {
      const sync = useScenarioSlice('sync')
      return <p>{Object.entries(sync.status).map(([surface, status]) => `${surface}: ${status._tag}`).join(', ')}</p>
    }
    const tagsOnly = { ...Sync, render: () => <TagsOnly /> }
    expect(scenarioStoryCheck(tagsOnly, meta, options).failures).toContainEqual(expect.objectContaining({
      _tag: 'DataDependence', check: 2, slice: 'sync', message: 'different wire states render identical content',
    }))
  })

  it('skips data contrasts for a reasoned invariant, but still requires its read', () => {
    const result = scenarioStoryCheck(Invariant, meta)
    expect(result._tag).toBe('Passed')
    expect(result.reads).toEqual(new Set(['sync']))
    expect(result.checks.find(({ check }) => check === 2)?._tag).toBe('Skipped')
    expect(result.checks.find(({ check }) => check === 3)?._tag).toBe('Skipped')
    const invalid = { ...Invariant, parameters: { scenario: { slices: ['sync'], invariant: { sync: ' ' } } } }
    expect(() => scenarioStoryCheck(invalid, meta)).toThrow('invariant sync requires a reason')
    const fixedInvariant = { ...Fixed, parameters: { scenario: { slices: ['roster'], invariant: { roster: 'Intentionally fixed layout' } } } }
    expect(scenarioStoryCheck(fixedInvariant, meta).failures.some(({ check }) => check === 1)).toBe(true)
  })

  it('treats withheld loading data as a contentless contrast side', () => {
    const result = scenarioStoryCheck(LoadingRoster, meta, { contrastPairs: { roster: [['default', 'loading']] } })
    expect(result._tag).toBe('Passed')
    expect(result.checks.find(({ check }) => check === 2)?._tag).toBe('Passed')
  })

  it('does not render backing roster or attention content while loading', () => {
    const Composed = composeStory(Good, meta, {
      decorators: [withScenario],
      globalTypes: scenarioGlobalTypes,
      initialGlobals: { scenario: 'loading', scenarioNow: ANCHOR_MS },
    })
    const markup = renderToStaticMarkup(<Composed />)
    expect(markup).toContain('Loading roster')
    expect(markup).toContain('Loading attention')
    const loading = loadWorld('loading', { now: ANCHOR_MS })
    expect(loading.slices.roster.state.agents.length).toBeGreaterThan(0)
    for (const agent of loading.slices.roster.state.agents) expect(markup).not.toContain(agent.name)
    const attentionItems = [...loading.slices.attention.state.attention, ...loading.slices.attention.state.messages]
    expect(attentionItems.length).toBeGreaterThan(0)
    for (const item of attentionItems) {
      expect(markup).not.toContain(item.title)
    }
  })

  it('rejects a loading side that leaks the other side’s data', () => {
    const LeakingRoster = () => {
      const roster = useScenarioSlice('roster')
      return <ul>{roster.state.agents.map((agent) => <li key={agent.id}>{agent.name}</li>)}</ul>
    }
    const leaking = { ...LoadingRoster, render: () => <LeakingRoster /> }
    const result = scenarioStoryCheck(leaking, meta, { contrastPairs: { roster: [['default', 'loading']] } })
    expect(result._tag).toBe('Failed')
    expect(result.failures).toContainEqual(expect.objectContaining({
      _tag: 'DataDependence', check: 2, slice: 'roster',
      message: expect.stringContaining('loading still renders the other side'),
    }))
  })

  it('renders one representative world contrast rather than every catalog pair', () => {
    let renders = 0
    const CountedRoster = () => {
      renders++
      const roster = useScenarioSlice('roster')
      return roster.loading ? <p>Loading roster</p> : <ul>{roster.state.agents.map((agent) => <li key={agent.id}>{agent.name}</li>)}</ul>
    }
    const counted = { ...LoadingRoster, render: () => <CountedRoster /> }
    const result = scenarioStoryCheck(counted, meta)
    expect(result._tag).toBe('Passed')
    expect(result.checks.find(({ check }) => check === 3)?._tag).toBe('Passed')
    // Baseline + two variant pairs + one world pair + URL/direct renders.
    expect(renders).toBeLessThanOrEqual(9)
  })

  it('skips world contrasts when declared visible content is identical', () => {
    const result = scenarioStoryCheck(LoadingRoster, meta, { args: { roster: 'empty' } })
    expect(result._tag).toBe('Passed')
    expect(result.checks.find(({ check }) => check === 2)?._tag).toBe('Passed')
    expect(result.checks.find(({ check }) => check === 3)?._tag).toBe('Skipped')
  })

  it('does not pair sync scripts whose projected status is identical', () => {
    const result = scenarioStoryCheck(Sync, meta, { args: { sync: 'requested' } })
    expect(result._tag).toBe('Passed')
    expect(result.checks.find(({ check }) => check === 2)?._tag).toBe('Passed')
    expect(result.checks.find(({ check }) => check === 3)?._tag).toBe('Skipped')
  })
})
