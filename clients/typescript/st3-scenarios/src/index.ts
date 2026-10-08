import { nowMs } from './kit/time.ts'
import { genericVariants } from './kit/variants.ts'
import { buildWorld, type World, type WorldDefinition } from './kit/world.ts'
import { worldDefinitions } from './worlds/index.ts'

export * from './kit/asciicast.ts'
export type * from './kit/cast.ts'
export * from './kit/rng.ts'
export * from './kit/slice.ts'
export type * from './kit/syncStatus.ts'
export * from './kit/time.ts'
export * from './kit/wire.ts'
export { buildWorld, UnknownVariantError } from './kit/world.ts'
export type * from './kit/world.ts'
export { agent } from './kit/factories/agent.ts'
export { diff, unifiedDiff } from './kit/factories/diff.ts'
export { terminalRun } from './kit/factories/terminalRun.ts'
export { terminalRecord } from './kit/factories/terminalRecord.ts'
export { toolCall } from './kit/factories/toolCall.ts'
export { turn, thread } from './kit/factories/turn.ts'
export { screenAt } from './kit/screen.ts'
export { genericVariants } from './kit/variants.ts'

export type WorldId = 'fleet-mid-refactor' | 'failed-sync-socket-dropped'

export const DEFAULT_WORLD: WorldId = 'fleet-mid-refactor'

export interface CatalogEntry {
  readonly id: string
  readonly title: string
  readonly narrative: string
  readonly seed: number
}

export const catalog: readonly CatalogEntry[] = worldDefinitions.map(({ id, title, narrative, seed }) => ({ id, title, narrative, seed }))

export class UnknownWorldError extends Error {}

const definition = (id: string): WorldDefinition => {
  const found = worldDefinitions.find((world) => world.id === id)
  if (found === undefined) throw new UnknownWorldError(`unknown scenario world ${id}`)
  return found
}

export interface LoadWorldOptions {
  /** The instant offsets are relative to; defaults to the load time. Screenshots and tests pin it. */
  readonly now?: number | Date | string
}

/** The world `id` with every instant relative to `now`. */
export const loadWorld = (id: WorldId | (string & {}), options: LoadWorldOptions = {}): World =>
  buildWorld(definition(id), genericVariants, nowMs(options.now ?? new Date()))
export { foldSlice, syncStatusAt, absolute } from './kit/fold.ts'
export { manualClock, realClock, type Clock, type ManualClock } from './clock.ts'
