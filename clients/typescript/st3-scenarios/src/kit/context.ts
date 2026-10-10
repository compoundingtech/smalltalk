import type { Cast } from './cast.ts'
import { fork, type Rng } from './rng.ts'
import type { TimeContext } from './time.ts'

/** What every factory receives: a generator, the world's cast and clock. */
export interface FactoryContext {
  readonly world: string
  readonly rng: Rng
  readonly cast: Cast
  readonly t: TimeContext
}

/** Same cast and clock, child generator for `label`. */
export const child = (ctx: FactoryContext, label: string): FactoryContext => ({ ...ctx, rng: fork(ctx.rng, label) })

/** `<family>/scenario-<world>-<rest>` for families without a dedicated form. */
export const scenarioId = (ctx: FactoryContext, family: string, rest: string | number): string =>
  `${family}/scenario-${ctx.world}-${rest}`
