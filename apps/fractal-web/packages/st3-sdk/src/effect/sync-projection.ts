import type { FollowFreshness } from './freshness.ts'
import type { FollowFailure } from './mod.ts'
import type { StaleReason, SyncStatus } from './sync-status.ts'

/** Missing metadata is unknown, never a made-up server reason or retry deadline. */
export const syncStatusFromFreshness = (
  freshness: FollowFreshness,
  lastLiveAt?: number,
): SyncStatus => {
  let reason: StaleReason
  switch (freshness._tag) {
    case 'Requested':
    case 'Live':
      return { _tag: freshness._tag, since: freshness.since }
    case 'Reconnecting':
      reason = freshness.nextAt === undefined
        ? { _tag: 'Unknown' }
        : { _tag: 'Reconnecting', attempt: freshness.attempt, nextAt: freshness.nextAt, issue: freshness.issue }
      break
    case 'Stale':
      switch (freshness.reason._tag) {
        case 'Resync':
          reason = freshness.reason.code === undefined || freshness.reason.code.length === 0 || freshness.reason.message === undefined
            ? { _tag: 'Unknown' }
            : { _tag: 'Resync', code: freshness.reason.code, message: freshness.reason.message, attempt: freshness.reason.attempt }
          break
        case 'Evicted':
          reason = { _tag: 'Evicted' }
          break
        case 'Unknown':
          reason = { _tag: 'Unknown' }
          break
      }
      break
    case 'Unknown':
      reason = { _tag: 'Unknown' }
      break
  }
  return { _tag: 'Stale', reason, ...(lastLiveAt === undefined ? {} : { lastLiveAt }) }
}

export const syncStatusFromFailure = (error: FollowFailure): SyncStatus => {
  if (error._tag === 'SubscriptionLimit')
    return { _tag: 'Failed', cause: { _tag: 'Local', kind: 'subscription-limit', detail: { cap: error.cap } } }
  if ((error._tag === 'Rejected' || error._tag === 'Attach') && error.code !== undefined && error.code.length > 0)
    return { _tag: 'Failed', cause: { _tag: 'Server', code: error.code, message: error.message } }
  return { _tag: 'Failed', cause: { _tag: 'Unknown' } }
}
