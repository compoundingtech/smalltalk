import type { WorldDefinition } from '../kit/world.ts'
import { syntheticFleetMidRefactor } from './fleetMidRefactor.ts'

/** The same refactor fleet, with the open-fail sync condition. */
export const failedSyncOpenFail: WorldDefinition = {
  ...syntheticFleetMidRefactor,
  id: 'failed-sync-open-fail',
  scope: syntheticFleetMidRefactor.id,
  title: 'Failed sync: open-fail',
  narrative: 'The refactor fleet retains its work and conversations while open-fail affects synchronization.',
  selected: { sync: 'open-fail' },
}
