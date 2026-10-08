import type { SyncStatus as KitSyncStatus } from '@smalltalk/fractal-ui/assistant-ui/sync'
import type { SyncStatus } from '@st3/sdk/effect'
import * as Equal from 'effect/Equal'
import * as Option from 'effect/Option'

type Same<TA, TB> = (<T>() => T extends TA ? 1 : 2) extends (<T>() => T extends TB ? 1 : 2) ? true : false
type Assert<T extends true> = T
/** The kit owns the portable union; the standalone SDK mirrors it exactly. */
export type SdkSyncStatusMatchesKit = Assert<Same<SyncStatus, KitSyncStatus>>

export interface FeedSyncObservation {
  readonly status: SyncStatus
  /** Local epoch milliseconds when this status or progress stage was first observed. */
  readonly observedAt: number
}

export interface FeedSync<TValue> {
  /** Most recent actual FollowEvent.Observed, retained across stale/failure transitions. */
  readonly last: Option.Option<{ readonly value: TValue; readonly observedAt: number }>
  readonly sync: FeedSyncObservation
}

const statusKey = (status: SyncStatus): string =>
  status._tag === 'Progress'
    ? `${status._tag}/${status.stage}`
    : status._tag === 'Stale'
      ? `${status._tag}/${status.reason._tag}`
      : status._tag

export const initialFeedSync = <TValue>(now: number, attempt = 1): FeedSync<TValue> => ({
  last: Option.none(),
  sync: { status: { _tag: 'Connecting', attempt, since: now }, observedAt: now },
})

/** Preserve SDK facts, including updated timestamps/details within the same observed stage. */
export const transitionFeedSync = <TValue>(
  current: FeedSync<TValue>,
  status: SyncStatus,
  now: number,
): FeedSync<TValue> =>
  Equal.equals(current.sync.status, status)
    ? current
    : {
        ...current,
        sync: {
          status,
          observedAt: statusKey(current.sync.status) === statusKey(status) ? current.sync.observedAt : now,
        },
      }

/** Recording retained content never manufactures Live: only the SDK status stream owns that verdict. */
export const observeFeedSync = <TValue>(
  current: FeedSync<TValue>,
  value: TValue,
  now: number,
): FeedSync<TValue> => ({ ...current, last: Option.some({ value, observedAt: now }) })
