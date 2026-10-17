import type { WorldDefinition } from '../kit/world.ts'
import { syntheticFleetMidRefactor } from './fleetMidRefactor.ts'

/** The same refactor fleet, with the non-client-response sync condition. */
export const failedSyncNonClientResponse: WorldDefinition = {
  ...syntheticFleetMidRefactor,
  id: 'failed-sync-non-client-response',
  scope: syntheticFleetMidRefactor.id,
  title: 'Failed sync: non-client-response',
  narrative: 'The refactor fleet retains its work and conversations while non-client-response affects synchronization.',
  selected: { sync: 'non-client-response' },
}
