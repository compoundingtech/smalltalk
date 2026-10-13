import type { WorldDefinition } from '../kit/world.ts'
import { syntheticFleetMidRefactor } from './fleetMidRefactor.ts'

/** The same refactor fleet, with the page-cursor-expired sync condition. */
export const failedSyncPageCursorExpired: WorldDefinition = {
  ...syntheticFleetMidRefactor,
  id: 'failed-sync-page-cursor-expired',
  scope: syntheticFleetMidRefactor.id,
  title: 'Failed sync: page-cursor-expired',
  narrative: 'The refactor fleet retains its work and conversations while page-cursor-expired affects synchronization.',
  selected: { sync: 'page-cursor-expired' },
}
