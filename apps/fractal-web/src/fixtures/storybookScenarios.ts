import { Agent, Attention, Mission, TimelineEntry, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import { Schema } from 'effect'
import * as Atom from 'effect/reactivity/Atom'
import { ANCHOR_MS, loadWorld, type World } from '../../../../clients/typescript/st3-scenarios/src/index.ts'
import { stLiveFrames } from '../conversation/fixtures.ts'
import { timelineItems } from '../conversation/fromTimeline.ts'
import { fixtureSource, type FixtureProjections } from '../data/fixtureSource.ts'
import { observed, unavailable, waiting, type ConversationPage, type DataSource } from '../data/source.ts'
import { fixtureProjections } from './projections.ts'

export const appStoryStates = ['loading', 'empty', 'populated', 'failed-tool', 'offline', 'unavailable', 'scripted-repair'] as const
export type AppStoryState = typeof appStoryStates[number]
export interface AppStoryFixture {
  readonly source: DataSource
  readonly projections: FixtureProjections
  readonly agentRef: string
  readonly agentName: string
}

/** Decode wire rows before using the existing timeline projector, just like the
 * scripted conversation fixtures. Accept any corpus world, not a fixed cast. */
export const projectScenarioWorld = (world: World): FixtureProjections => ({
  now: world.now,
  events: [],
  agents: world.slices.roster.state.agents.map(value => decodeUnknownSync(Agent, 'strict')(value)),
  missions: world.slices.details.state.missions.map(value => decodeUnknownSync(Mission, 'strict')(value)),
  attention: world.slices.attention.state.attention.map(value => decodeUnknownSync(Attention, 'strict')(value)),
  conversations: Object.fromEntries(world.slices.conversation.state.threads.map(thread => {
    const entries = thread.items.map(value => decodeUnknownSync(TimelineEntry, 'strict')(value))
    const page: ConversationPage = {
      items: timelineItems([{ replace: true, entries, hasMore: thread.has_more }]),
      hasOlder: thread.has_more,
    }
    return [thread.agent, page]
  })),
  // These books exercise thread boundaries, not terminal or resource protocols.
  terminals: {},
  envelopes: {},
  usage: { _tag: 'undeclared' },
})

export const scenarioForAppStory = (state: AppStoryState): World => {
  const base = loadWorld('fleet-mid-refactor', { now: ANCHOR_MS })
  return state === 'loading' ? base.with({ conversation: 'loading' })
    : state === 'empty' ? base.with({ conversation: 'empty' }) : base
}

export const createAppStoryFixture = (state: AppStoryState): AppStoryFixture => {
  const scenario = scenarioForAppStory(state)
  let projections = projectScenarioWorld(scenario)
  // The default builder thread contains a failed shell command; the second
  // thread provides a different populated session rather than a repeated template.
  const thread = scenario.slices.conversation.state.threads[state === 'populated' ? 1 : 0]
  if (thread === undefined) throw new Error('App story corpus has no selected thread')
  let agentRef: string = thread.agent
  let agentName: string = projections.agents.find(agent => agent.id === agentRef)?.name ?? agentRef
  if (state === 'scripted-repair') {
    // Reuse the existing world/script. This stream has only production schema
    // entries (no proposed reasoning model). Roundtrip through the wire codec.
    const entries = stLiveFrames.flatMap(frame => frame.entries).map(entry => {
      if (!('body' in entry) || entry.type === 'reasoning') throw new Error('A proposed or unrecognized entry is not a production fixture')
      return decodeUnknownSync(TimelineEntry, 'strict')(Schema.encodeSync(TimelineEntry)(entry))
    })
    agentRef = 'agent/build-host-a/workbench-shell'
    agentName = fixtureProjections.agents.find(agent => agent.id === agentRef)?.name ?? agentRef
    projections = { ...fixtureProjections, conversations: {
      [agentRef]: { items: timelineItems([{ replace: true, entries, hasMore: true }]), hasOlder: true },
    } }
  }
  projections = { ...projections, agents: [
    ...projections.agents.filter(agent => agent.id === agentRef),
    ...projections.agents.filter(agent => agent.id !== agentRef),
  ] }
  const page = projections.conversations[agentRef]
  if (page === undefined) throw new Error('App story corpus has no conversation page')
  const source = fixtureSource({
    world: projections,
    overrides: {
      conversation: { [agentRef]: state === 'loading' ? waiting
        : state === 'unavailable' ? unavailable({ reason: 'failed', detail: 'Fictional fixture read refused', code: 'not-found' })
        : observed({ value: page, freshness: state === 'offline' ? 'stale' : 'live' }) },
      ...(state === 'offline' ? {
        connection: { _tag: 'Reconnecting', attempt: 1, issue: 'The browser is offline', nextAt: projections.now + 5000 },
        agents: observed({ value: projections.agents, freshness: 'stale' }),
      } : {}),
    },
  })
  return { source: state === 'offline' ? { ...source, network: Atom.make({ _tag: 'Offline' as const }) } : source, projections, agentRef, agentName }
}
