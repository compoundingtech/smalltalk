import type { FollowFailure } from '@st3/sdk/effect'
import * as Option from 'effect/Option'

export type FeedSyncFailure =
  | FollowFailure
  | { readonly _tag: 'ConnectionRejected'; readonly message: string }

export type FeedSyncStatus =
  | { readonly _tag: 'Connecting'; readonly attempt: number }
  | { readonly _tag: 'Requested' }
  | { readonly _tag: 'Live' }
  | { readonly _tag: 'Stale'; readonly code?: string; readonly message?: string }
  | {
      readonly _tag: 'Failed'
      readonly failure: FeedSyncFailure
    }

export interface FeedSyncObservation {
  readonly status: FeedSyncStatus
  /** Local epoch milliseconds when this status transition was observed. */
  readonly observedAt: number
}

export interface FeedSync<TValue> {
  /** Most recent actual FollowEvent.Observed, retained across stale/failure transitions. */
  readonly last: Option.Option<{ readonly value: TValue; readonly observedAt: number }>
  readonly sync: FeedSyncObservation
}

const statusKey = (status: FeedSyncStatus): string => {
  switch (status._tag) {
    case 'Connecting':
      return 'Connecting:' + status.attempt
    case 'Requested':
    case 'Live':
      return status._tag
    case 'Stale':
      return 'Stale:' + (status.code ?? '') + ':' + (status.message ?? '')
    case 'Failed': {
      const failure = status.failure
      if (failure._tag === 'SubscriptionLimit') return 'Failed:SubscriptionLimit:' + failure.cap
      if (failure._tag === 'ConnectionRejected') return 'Failed:ConnectionRejected:' + failure.message
      const code = 'code' in failure ? failure.code ?? '' : ''
      const message = 'message' in failure ? failure.message : ''
      return 'Failed:' + failure._tag + ':' + code + ':' + message
    }
  }
}

export const initialFeedSync = <TValue>(now: number, attempt = 1): FeedSync<TValue> => ({
  last: Option.none(),
  sync: { status: { _tag: 'Connecting', attempt }, observedAt: now },
})

export const transitionFeedSync = <TValue>(
  current: FeedSync<TValue>,
  status: FeedSyncStatus,
  now: number,
): FeedSync<TValue> =>
  statusKey(current.sync.status) === statusKey(status)
    ? current
    : { ...current, sync: { status, observedAt: now } }

export const observeFeedSync = <TValue>(
  current: FeedSync<TValue>,
  value: TValue,
  now: number,
): FeedSync<TValue> => {
  const next = transitionFeedSync(current, { _tag: 'Live' }, now)
  return { ...next, last: Option.some({ value, observedAt: now }) }
}
