import type { WorldDefinition } from '../kit/world.ts'
import { fleetMidRefactor } from './fleetMidRefactor.ts'

/** The same refactor fleet, with the subscription-error sync condition. */
export const failedSyncSubscriptionError: WorldDefinition = {
  ...fleetMidRefactor,
  id: 'failed-sync-subscription-error',
  scope: fleetMidRefactor.id,
  title: 'Failed sync: subscription-error',
  narrative: 'The refactor fleet retains its work and conversations while subscription-error affects synchronization.',
  selected: { sync: 'subscription-error' },
}
