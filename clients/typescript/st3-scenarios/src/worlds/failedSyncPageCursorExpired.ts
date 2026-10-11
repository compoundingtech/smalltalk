import type { WorldDefinition } from '../kit/world.ts'
import { fleetMidRefactor } from './fleetMidRefactor.ts'

/** The same refactor fleet, with the page-cursor-expired sync condition. */
export const failedSyncPageCursorExpired: WorldDefinition = {
  ...fleetMidRefactor,
  id: 'failed-sync-page-cursor-expired',
  scope: fleetMidRefactor.id,
  title: 'Failed sync: page-cursor-expired',
  narrative: 'The refactor fleet retains its work and conversations while page-cursor-expired affects synchronization.',
  selected: { sync: 'page-cursor-expired' },
}
