import { describe, expect, it } from 'vitest'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { SLICE_KINDS } from '../../../../clients/typescript/st3-scenarios/src/index.ts'
import { decodeSlice } from '../../../../clients/typescript/st3-scenarios/scripts/decode.ts'
import { scanContent, scanIdentities } from '../../../../clients/typescript/st3-scenarios/scripts/scan.ts'
import { appStoryStates, createAppStoryFixture, scenarioForAppStory } from './storybookScenarios.ts'

describe('production app Storybook scenarios', () => {
  for (const state of appStoryStates) {
    it(`decodes and privacy-gates ${state} before projecting into fixtureSource`, () => {
      const world = scenarioForAppStory(state)
      for (const kind of SLICE_KINDS) {
        expect(decodeSlice(world.id, world.slices[kind])).toEqual([])
        const wire = JSON.stringify(world.slices[kind])
        expect(scanContent(`${state}/${kind}`, wire)).toEqual([])
        expect(scanIdentities(`${state}/${kind}`, wire)).toEqual([])
      }
      const fixture = createAppStoryFixture(state)
      expect(scanContent(state, JSON.stringify(fixture.projections.conversations))).toEqual([])
      const registry = AtomRegistry.make()
      try {
        expect(registry.get(fixture.source.now)).toBe(fixture.projections.now)
        expect(registry.get(fixture.source.conversation(fixture.agentRef))._tag)
          .toBe(state === 'loading' ? 'Waiting' : state === 'unavailable' ? 'Unavailable' : 'Observed')
        expect(fixture.projections.agents[0]?.id).toBe(fixture.agentRef)
        if (state === 'failed-tool') {
          expect(fixture.projections.conversations[fixture.agentRef]?.items.some(item => item._tag === 'ToolCall' && item.status === 'error')).toBe(true)
        }
        if (state === 'offline') {
          expect(registry.get(fixture.source.connection)._tag).toBe('Reconnecting')
          expect(registry.get(fixture.source.conversation(fixture.agentRef))).toMatchObject({ freshness: 'stale' })
        }
      } finally { registry.dispose() }
    })
  }
})
