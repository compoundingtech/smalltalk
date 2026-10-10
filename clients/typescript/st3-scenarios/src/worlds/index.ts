import type { WorldDefinition } from '../kit/world.ts'
import { failedSyncSocketDropped } from './failedSyncSocketDropped.ts'
import { fleetMidRefactor } from './fleetMidRefactor.ts'

/** Every world in catalog order; the first is the default. */
export const worldDefinitions: readonly WorldDefinition[] = [fleetMidRefactor, failedSyncSocketDropped]
