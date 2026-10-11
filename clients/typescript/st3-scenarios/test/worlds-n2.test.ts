import type { Agent } from '@smalltalk/st3-client'
import { describe, expect, it } from 'vitest'
import { catalog, loadWorld } from '../src/index.ts'
import { decodeSlice } from '../scripts/decode.ts'
import { foldSlice } from '../src/kit/fold.ts'
import { SLICE_KINDS, type AnySlice } from '../src/kit/slice.ts'
import { ANCHOR_MS, parseTimestamp } from '../src/kit/time.ts'
import { genericVariants } from '../src/kit/variants.ts'
import { buildWorld, type World } from '../src/kit/world.ts'
import { firstRunOnboarding } from '../src/worlds/firstRunOnboarding.ts'
import { mergeConflictStandoff } from '../src/worlds/mergeConflictStandoff.ts'

for (const definition of [firstRunOnboarding, mergeConflictStandoff]) {
  describe(definition.id, () => {
    for (const now of [ANCHOR_MS, ANCHOR_MS + 123_456_789]) {
      it(`strict-decodes initial and every folded event at ${now}`, () => {
        const world = buildWorld(definition, genericVariants, now)
        for (const kind of SLICE_KINDS) {
          const slice = world.slices[kind]
          expect(decodeSlice(world.id, slice)).toEqual([])
          for (const event of slice.timeline) expect(decodeSlice(world.id, foldSlice(slice, event.at_ms) as AnySlice)).toEqual([])
        }
      })
    }
  })
}

/** Every roster upsert that changes an agent's state, or its joined work's state, restarts that clock at the event. */
const staleTransitions = (world: World): string[] => {
  const roster = world.slices.roster
  const known = new Map<string, Agent>(roster.state.agents.map((agent) => [agent.id, agent]))
  const stale: string[] = []
  const restarted = (since: string | null | undefined, at: number) => typeof since === 'string' && parseTimestamp(since) === at
  for (const event of roster.timeline) {
    if (event._tag !== 'changes') continue
    const at = world.now + event.at_ms
    const upserted = new Set(event.upserts.map((upsert) => upsert.id))
    for (const agent of foldSlice(roster, event.at_ms).state.agents.filter((candidate) => upserted.has(candidate.id))) {
      const before = known.get(agent.id)
      if (before !== undefined && before.state !== agent.state && !restarted(agent.since, at)) stale.push(`${world.id}@${event.at_ms}: ${agent.id} since`)
      for (const work of agent.current_work ?? []) {
        const previous = before?.current_work?.find((candidate) => candidate.id === work.id)
        if (previous?.state !== work.state && !restarted(work.since, at)) stale.push(`${world.id}@${event.at_ms}: ${work.id} since`)
      }
      known.set(agent.id, agent)
    }
  }
  return stale
}

it('restarts agent and joined-work state clocks at each roster transition in every world', () => {
  expect(catalog.flatMap(({ id }) => staleTransitions(loadWorld(id, { now: ANCHOR_MS })))).toEqual([])
})

it('onboarding has no hidden resources at zero and becomes a green first run', () => {
  const world = buildWorld(firstRunOnboarding, genericVariants, ANCHOR_MS)
  expect(foldSlice(world.slices.roster, 0).state).toEqual({ agents: [], runtimes: [], machines: [], order: [] })
  expect(foldSlice(world.slices.details, 0).state).toEqual({ missions: [], work: [] })
  expect(foldSlice(world.slices.attention, 0).state).toEqual({ attention: [], messages: [] })
  expect(foldSlice(world.slices.conversation, 0).state).toEqual({ threads: [] })
  expect(foldSlice(world.slices.terminal, 0).state).toEqual({ terminals: [] })
  expect(foldSlice(world.slices.roster, 1_000).state.machines).toHaveLength(1)
  expect(foldSlice(world.slices.roster, 2_000).state.agents).toHaveLength(1)
  expect(foldSlice(world.slices.attention, 3_000).state.attention[0]?.title).toBe('Launch your first mission')
  const roster = foldSlice(world.slices.roster, Infinity).state
  const conversation = foldSlice(world.slices.conversation, Infinity).state
  const terminal = foldSlice(world.slices.terminal, Infinity).state
  expect(roster.runtimes).toHaveLength(1)
  expect(conversation.threads[0]?.agent).toBe(roster.agents[0]?.id)
  expect(terminal.terminals[0]?.owner).toBe(roster.agents[0]?.id)
  expect(JSON.stringify(conversation)).toContain('3 tests passed')
  expect(JSON.stringify(terminal)).toContain('3 tests passed')
  expect(foldSlice(world.slices.details, Infinity).state.work[0]?.state).toBe('completed')
  // Planted negative witness: folding before creation cannot masquerade as the final state.
  expect(foldSlice(world.slices.conversation, 4_999).state.threads).not.toEqual(conversation.threads)
})

it('both agents expose the same three-way conflict and the approved combined patch lands', () => {
  const world = buildWorld(mergeConflictStandoff, genericVariants, ANCHOR_MS)
  const initial = world.slices.conversation.state.threads
  expect(initial).toHaveLength(2)
  for (const thread of initial) {
    const content = JSON.stringify(thread.items)
    expect(content).toContain('packages/cache/policy.ts')
    expect(content).toContain('||||||| base')
    expect(content).toContain('ttl: 60, stale: false')
    expect(content).toContain('ttl: 300, stale: false')
    expect(content).toContain('ttl: 60, stale: true')
  }
  const cards = world.slices.attention.state.attention
  expect(cards).toHaveLength(2)
  expect(cards[0]?.detail).toEqual(cards[1]?.detail)
  expect(cards.every((card) => card.actions.includes('review.approve'))).toBe(true)
  const attention = foldSlice(world.slices.attention, 2_000).state
  expect(attention.attention).toEqual([])
  expect(attention.messages.every((message) => message.from === world.cast.people[0]?.id)).toBe(true)
  const final = foldSlice(world.slices.conversation, Infinity).state
  expect(JSON.stringify(final)).toContain('ttl: 300, stale: true')
  expect(JSON.stringify(final)).toContain('combined patch landed as c0ffee1')
  expect(foldSlice(world.slices.details, Infinity).state.work.every((work) => work.state === 'completed')).toBe(true)
  // Planted negative witness: neither unreviewed branch already contains the combined patch.
  expect(JSON.stringify(initial)).not.toContain('ttl: 300, stale: true')
})
