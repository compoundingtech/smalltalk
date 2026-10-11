import { revise } from '../kit/factories/turn.ts'
import * as resources from '../kit/resources.ts'
import { conversationVariants } from '../kit/variants/conversation.ts'
import { rosterVariants } from '../kit/variants/roster.ts'
import { liveSync } from '../kit/variants.ts'
import type { CastAgentSpec } from '../kit/cast.ts'
import type { ConversationEvent } from '../kit/slice.ts'
import type { WorldDefinition } from '../kit/world.ts'
import { fleetMidRefactor } from './fleetMidRefactor.ts'

const SEED = 24
const roles: readonly CastAgentSpec['role'][] = ['builder', 'reviewer', 'migrator', 'docs', 'tester', 'release']

/** A complete fleet, but only the scripted conversation window is committed. */
export const huge: WorldDefinition = {
  id: 'huge',
  title: 'Huge fleet and conversation',
  narrative: 'A thousand agents on six hosts carry twenty migration missions; one seat has ten thousand turns of history, with bounded live windows and older pages.',
  seed: SEED,
  cast: {
    project: 'atlas',
    agents: Array.from({ length: 1_000 }, (_, index): CastAgentSpec => {
      const role = roles[index % roles.length]!
      return { key: `${role}-${index + 1}`, role, name: `Atlas ${role} ${index + 1}` }
    }),
    hosts: 6,
    people: 2,
    missions: Array.from({ length: 20 }, (_, index) => ({
      slug: `migration-${index + 1}`,
      title: `Preserve session-aware callers in migration group ${index + 1}`,
      steps: Array.from({ length: 50 }, (_, step) => `Update and validate caller batch ${step + 1}`),
    })),
  },
  slices: (ctx) => {
    const inherited = fleetMidRefactor.slices(ctx)
    const source = { _tag: 'synthetic', seed: SEED } as const
    const base = { ...inherited, conversation: { ...inherited.conversation, source } }
    const roster = { ...rosterVariants.huge!(ctx, base), source }
    const initial = { ...conversationVariants.huge!(ctx, base), source }
    const selected = initial.state.threads[0]!
    const streaming = selected.items.at(-2)!
    const partial = { ...streaming, final: false }
    const second = revise(ctx, partial, 1_000, { media_type: 'text/markdown', text: 'The final caller batch is validated; I am checking its migration handoff.' }, false)
    const final = revise(ctx, second, 2_000, { media_type: 'text/markdown', text: 'The caller tests and migration handoff are checked. The fleet can continue with the next group.' }, true)
    const timeline: ConversationEvent[] = [
      { _tag: 'entries', at_ms: 1_000, store: 0, agent: selected.agent, items: [second] },
      { _tag: 'entries', at_ms: 2_000, store: 0, agent: selected.agent, items: [final] },
    ]
    const conversation = { ...initial, state: { threads: [{ ...selected, items: selected.items.map((item) => item.id === partial.id ? partial : item) }] }, timeline }
    const details = {
      ...inherited.details,
      source,
      state: {
        missions: ctx.cast.missions.map((mission) => resources.mission(ctx, mission, 'running', -60_000)),
        work: ctx.cast.missions.flatMap((mission, missionIndex) => mission.steps.map((_step, step) => resources.work(ctx, {
          mission, step, state: 'claimed', updatedMs: -60_000,
          claimant: ctx.cast.agents[step * ctx.cast.missions.length + missionIndex],
          goals: ['Preserve the session argument and pass the caller tests'],
        }))),
      },
      timeline: [],
    }
    return {
      roster,
      details,
      attention: { ...inherited.attention, source, state: { attention: [], messages: [] }, timeline: [] },
      conversation,
      terminal: { ...inherited.terminal, source, timeline: [] },
      sync: { ...liveSync(ctx, { conversation }), source },
    }
  },
}
