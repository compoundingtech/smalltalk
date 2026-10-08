import type { FactoryContext } from '../context.ts'
import * as resources from '../resources.ts'
import type { DetailsState } from '../slice.ts'
import type { VariantTable } from '../world.ts'
import { cleared } from './shared.ts'

const workState = (ctx: FactoryContext, failed: boolean): DetailsState => ({
  missions: ctx.cast.missions.map((mission) => resources.mission(ctx, mission, failed ? 'failed' : 'running', -2_400_000)),
  work: ctx.cast.missions.flatMap((mission) => mission.steps.map((_step, index) => {
    const value = resources.work(ctx, { mission, step: index, state: failed ? 'failed' : 'blocked', updatedMs: -2_400_000, claimant: ctx.cast.agents[index % ctx.cast.agents.length], blockedReason: failed ? 'Typecheck failed: the caller omits the session argument. Repair the signature and retry.' : 'Waiting for the preceding API review before migrating callers.', goals: ['Typecheck the migrated caller and preserve its session'] })
    const predecessor = mission.steps[index - 1]
    return { ...value, blockers: predecessor === undefined ? [] : [predecessor.work] }
  })),
})

export const detailsVariants: VariantTable['details'] = {
  empty: (_ctx, base) => cleared(base.details, { missions: [], work: [] }),
  loading: (_ctx, base) => ({ ...base.details, loading: true }),
  stalled: (ctx, base) => cleared(base.details, workState(ctx, false)),
  failed: (ctx, base) => cleared(base.details, workState(ctx, true)),
}
