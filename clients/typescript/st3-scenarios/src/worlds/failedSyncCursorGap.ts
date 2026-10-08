import type { WorldDefinition } from '../kit/world.ts'
import { fleetMidRefactor } from './fleetMidRefactor.ts'

/** The same refactor fleet, with the cursor-gap sync condition. */
export const failedSyncCursorGap: WorldDefinition = {
  ...fleetMidRefactor,
  id: 'failed-sync-cursor-gap',
  scope: fleetMidRefactor.id,
  title: 'Failed sync: cursor-gap',
  narrative: 'The refactor fleet retains its work and conversations while cursor-gap affects synchronization.',
  selected: { sync: 'cursor-gap' },
}
