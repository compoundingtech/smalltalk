import type { WorldDefinition } from '../kit/world.ts'
import { syntheticFleetMidRefactor } from './fleetMidRefactor.ts'

/** The same refactor fleet, with the rate-limited sync condition. */
export const failedSyncRateLimited: WorldDefinition = {
  ...syntheticFleetMidRefactor,
  id: 'failed-sync-rate-limited',
  scope: syntheticFleetMidRefactor.id,
  title: 'Failed sync: rate-limited',
  narrative: 'The refactor fleet retains its work and conversations while rate-limited affects synchronization.',
  selected: { sync: 'rate-limited' },
}
