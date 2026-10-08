import type { WorldDefinition } from '../kit/world.ts'
import { ciFailingFix } from './ciFailingFix.ts'
import { empty } from './empty.ts'
import { failedSyncCapabilityAbsent } from './failedSyncCapabilityAbsent.ts'
import { failedSyncCursorGap } from './failedSyncCursorGap.ts'
import { failedSyncForbidden } from './failedSyncForbidden.ts'
import { failedSyncNonClientResponse } from './failedSyncNonClientResponse.ts'
import { failedSyncOpenFail } from './failedSyncOpenFail.ts'
import { failedSyncPageCursorExpired } from './failedSyncPageCursorExpired.ts'
import { failedSyncRateLimited } from './failedSyncRateLimited.ts'
import { failedSyncResyncCoded } from './failedSyncResyncCoded.ts'
import { failedSyncSocketDropped } from './failedSyncSocketDropped.ts'
import { failedSyncSubscriptionError } from './failedSyncSubscriptionError.ts'
import { failedSyncSubscriptionLimitLegacy } from './failedSyncSubscriptionLimitLegacy.ts'
import { firstRunOnboarding } from './firstRunOnboarding.ts'
import { fleetMidRefactor } from './fleetMidRefactor.ts'
import { loading } from './loading.ts'
import { huge } from './huge.ts'
import { longDebugRabbitHole } from './longDebugRabbitHole.ts'
import { mergeConflictStandoff } from './mergeConflictStandoff.ts'
import { offlineReconnectStorm } from './offlineReconnectStorm.ts'
import { releaseCandidatePolish } from './releaseCandidatePolish.ts'
import { replicationDiverged } from './replicationDiverged.ts'
import { unicode } from './unicode.ts'
import { unknownFields } from './unknownFields.ts'

/** Every world in catalog order; the first is the default. */
export const worldDefinitions: readonly WorldDefinition[] = [
  fleetMidRefactor,
  ciFailingFix,
  longDebugRabbitHole,
  firstRunOnboarding,
  mergeConflictStandoff,
  offlineReconnectStorm,
  releaseCandidatePolish,
  empty,
  loading,
  unknownFields,
  failedSyncSocketDropped,
  failedSyncOpenFail,
  failedSyncResyncCoded,
  failedSyncForbidden,
  failedSyncNonClientResponse,
  failedSyncSubscriptionError,
  failedSyncSubscriptionLimitLegacy,
  failedSyncCapabilityAbsent,
  failedSyncRateLimited,
  failedSyncCursorGap,
  failedSyncPageCursorExpired,
  replicationDiverged,
  huge,
  unicode,
]
