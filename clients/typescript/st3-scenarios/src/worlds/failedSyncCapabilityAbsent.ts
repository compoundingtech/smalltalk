import type { WorldDefinition } from '../kit/world.ts'
import { fleetMidRefactor } from './fleetMidRefactor.ts'

/** The same refactor fleet, with the capability-absent sync condition. */
export const failedSyncCapabilityAbsent: WorldDefinition = {
  ...fleetMidRefactor,
  id: 'failed-sync-capability-absent',
  scope: fleetMidRefactor.id,
  title: 'Failed sync: capability-absent',
  narrative: 'The refactor fleet retains its work and conversations while capability-absent affects synchronization.',
  selected: { sync: 'capability-absent' },
}
