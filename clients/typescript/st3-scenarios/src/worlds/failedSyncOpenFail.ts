import type { WorldDefinition } from '../kit/world.ts'
import { fleetMidRefactor } from './fleetMidRefactor.ts'

/** The same refactor fleet, with the open-fail sync condition. */
export const failedSyncOpenFail: WorldDefinition = {
  ...fleetMidRefactor,
  id: 'failed-sync-open-fail',
  scope: fleetMidRefactor.id,
  title: 'Failed sync: open-fail',
  narrative: 'The refactor fleet retains its work and conversations while open-fail affects synchronization.',
  selected: { sync: 'open-fail' },
}
