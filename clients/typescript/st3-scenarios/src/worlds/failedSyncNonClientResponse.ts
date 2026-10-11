import type { WorldDefinition } from '../kit/world.ts'
import { fleetMidRefactor } from './fleetMidRefactor.ts'

/** The same refactor fleet, with the non-client-response sync condition. */
export const failedSyncNonClientResponse: WorldDefinition = {
  ...fleetMidRefactor,
  id: 'failed-sync-non-client-response',
  scope: fleetMidRefactor.id,
  title: 'Failed sync: non-client-response',
  narrative: 'The refactor fleet retains its work and conversations while non-client-response affects synchronization.',
  selected: { sync: 'non-client-response' },
}
