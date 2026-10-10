import type { WorldDefinition } from '../kit/world.ts'
import { fleetMidRefactor } from './fleetMidRefactor.ts'

/** `fleet-mid-refactor` whose collections socket drops after `Live` and keeps failing to reopen. */
export const failedSyncSocketDropped: WorldDefinition = {
  ...fleetMidRefactor,
  id: 'failed-sync-socket-dropped',
  scope: fleetMidRefactor.id,
  title: 'Failed sync: socket dropped',
  narrative: 'The fleet mid-refactor, seen while the collections socket is down and every reopen fails.',
  selected: { sync: 'socket-dropped' },
}
