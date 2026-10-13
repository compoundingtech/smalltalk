import type { WorldDefinition } from '../kit/world.ts'
import { syntheticFleetMidRefactor } from './fleetMidRefactor.ts'

/** The same refactor fleet, with the resync-coded sync condition. */
export const failedSyncResyncCoded: WorldDefinition = {
  ...syntheticFleetMidRefactor,
  id: 'failed-sync-resync-coded',
  scope: syntheticFleetMidRefactor.id,
  title: 'Failed sync: resync-coded',
  narrative: 'The refactor fleet retains its work and conversations while resync-coded affects synchronization.',
  selected: { sync: 'resync-coded' },
}
