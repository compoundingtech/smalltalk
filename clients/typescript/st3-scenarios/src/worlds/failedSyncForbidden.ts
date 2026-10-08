import type { WorldDefinition } from '../kit/world.ts'
import { fleetMidRefactor } from './fleetMidRefactor.ts'

/** The same refactor fleet, with the forbidden sync condition. */
export const failedSyncForbidden: WorldDefinition = {
  ...fleetMidRefactor,
  id: 'failed-sync-forbidden',
  scope: fleetMidRefactor.id,
  title: 'Failed sync: forbidden',
  narrative: 'The refactor fleet retains its work and conversations while forbidden affects synchronization.',
  selected: { sync: 'forbidden' },
}
