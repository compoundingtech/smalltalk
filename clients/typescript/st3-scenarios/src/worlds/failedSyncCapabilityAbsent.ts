import type { WorldDefinition } from '../kit/world.ts'
import { syntheticFleetMidRefactor } from './fleetMidRefactor.ts'

/** The same refactor fleet, with the capability-absent sync condition. */
export const failedSyncCapabilityAbsent: WorldDefinition = {
  ...syntheticFleetMidRefactor,
  id: 'failed-sync-capability-absent',
  scope: syntheticFleetMidRefactor.id,
  title: 'Failed sync: capability-absent',
  narrative: 'The refactor fleet retains its work and conversations while capability-absent affects synchronization.',
  selected: { sync: 'capability-absent' },
}
