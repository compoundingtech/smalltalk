import type { WorldDefinition } from '../kit/world.ts'
import { syntheticFleetMidRefactor } from './fleetMidRefactor.ts'

/** The same refactor fleet, with the forbidden sync condition. */
export const failedSyncForbidden: WorldDefinition = {
  ...syntheticFleetMidRefactor,
  id: 'failed-sync-forbidden',
  scope: syntheticFleetMidRefactor.id,
  title: 'Failed sync: forbidden',
  narrative: 'The refactor fleet retains its work and conversations while forbidden affects synchronization.',
  selected: { sync: 'forbidden' },
}
