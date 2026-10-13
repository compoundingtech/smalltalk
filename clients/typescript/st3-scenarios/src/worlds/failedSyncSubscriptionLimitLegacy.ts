import type { WorldDefinition } from '../kit/world.ts'
import { syntheticFleetMidRefactor } from './fleetMidRefactor.ts'

/** The same refactor fleet, with the subscription-limit-legacy sync condition. */
export const failedSyncSubscriptionLimitLegacy: WorldDefinition = {
  ...syntheticFleetMidRefactor,
  id: 'failed-sync-subscription-limit-legacy',
  scope: syntheticFleetMidRefactor.id,
  title: 'Failed sync: subscription-limit-legacy',
  narrative: 'The refactor fleet retains its work and conversations while subscription-limit-legacy affects synchronization.',
  selected: { sync: 'subscription-limit-legacy' },
}
