import type { WorldDefinition } from '../kit/world.ts'
import { fleetMidRefactor } from './fleetMidRefactor.ts'

/** The same refactor fleet, with the subscription-limit-legacy sync condition. */
export const failedSyncSubscriptionLimitLegacy: WorldDefinition = {
  ...fleetMidRefactor,
  id: 'failed-sync-subscription-limit-legacy',
  scope: fleetMidRefactor.id,
  title: 'Failed sync: subscription-limit-legacy',
  narrative: 'The refactor fleet retains its work and conversations while subscription-limit-legacy affects synchronization.',
  selected: { sync: 'subscription-limit-legacy' },
}
