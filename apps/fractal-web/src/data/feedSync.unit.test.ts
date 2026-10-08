import { describe, expect, it } from 'vitest'
import * as Option from 'effect/Option'
import {
  AttachFailure,
  Rejected,
  SubscriptionLimit,
  syncStatusFromFailure,
  syncStatusFromFreshness,
  type FollowFreshness,
  type SyncStatus,
} from '@st3/sdk/effect'

import { initialFeedSync, observeFeedSync, transitionFeedSync } from './feedSync.ts'


describe('feed sync observations', () => {
  it('retains actual observations across stale and failed SDK verdicts', () => {
    const initial = initialFeedSync<readonly string[]>(100)
    expect(Option.isNone(initial.last)).toBe(true)
    const requested = transitionFeedSync(initial, { _tag: 'Requested', since: 120 }, 120)
    const row = observeFeedSync(requested, ['row'], 150)
    expect(row.sync.status).toEqual({ _tag: 'Requested', since: 120 })
    const live = transitionFeedSync(row, { _tag: 'Live', since: 150 }, 150)
    const stale = transitionFeedSync(live, {
      _tag: 'Stale',
      reason: { _tag: 'Resync', code: 'resync-required', message: 'snapshot required', attempt: 1 },
      lastLiveAt: 150,
    }, 170)
    const failedStatus = syncStatusFromFailure(new Rejected({ code: 'forbidden', message: 'permission denied' }))
    const failed = transitionFeedSync(stale, failedStatus, 190)
    expect(Option.getOrUndefined(failed.last)).toEqual({ value: ['row'], observedAt: 150 })
    expect(failed.sync).toEqual({ status: failedStatus, observedAt: 190 })
    expect(failed.sync.status).toBe(failedStatus)
  })

  it('keeps transition clocks within a stage but preserves updated SDK facts', () => {
    const connecting = initialFeedSync<string>(100)
    const requested = transitionFeedSync(connecting, { _tag: 'Requested', since: 130 }, 130)
    const repeated = transitionFeedSync(requested, { _tag: 'Requested', since: 160 }, 160)
    expect(repeated.sync).toEqual({ status: { _tag: 'Requested', since: 160 }, observedAt: 130 })
    const progress: SyncStatus = { _tag: 'Progress', stage: 'reading', elapsedMs: 500, stageSince: 160, reportedAt: 180 }
    const reading = transitionFeedSync(repeated, progress, 180)
    const next = transitionFeedSync(reading, { ...progress, reportedAt: 220, done: 2, total: 4 }, 220)
    expect(next.sync.observedAt).toBe(180)
    expect(next.sync.status).toMatchObject({ reportedAt: 220, done: 2, total: 4 })
    const observed = observeFeedSync(next, 'second', 230)
    expect(observed.sync).toBe(next.sync)
    expect(Option.getOrUndefined(observed.last)).toEqual({ value: 'second', observedAt: 230 })
  })

  it('does not invent metadata for an unknown stale verdict', () => {
    const row = observeFeedSync(initialFeedSync<string>(100), 'row', 120)
    const status: SyncStatus = { _tag: 'Stale', reason: { _tag: 'Unknown' } }
    const stale = transitionFeedSync(row, status, 130)
    expect(stale.sync.status).toBe(status)
    expect(Option.getOrUndefined(stale.last)).toEqual({ value: 'row', observedAt: 120 })
  })

  it('uses SDK freshness mapping without app timestamps or diagnostic fields', () => {
    const cases: readonly [FollowFreshness, SyncStatus][] = [
      [{ _tag: 'Requested', since: 1 }, { _tag: 'Requested', since: 1 }],
      [{ _tag: 'Live', since: 2 }, { _tag: 'Live', since: 2 }],
      [
        { _tag: 'Reconnecting', attempt: 3, nextAt: 400, issue: 'socket dropped' },
        { _tag: 'Stale', reason: { _tag: 'Reconnecting', attempt: 3, nextAt: 400, issue: 'socket dropped' } },
      ],
      [{ _tag: 'Reconnecting', attempt: 3, issue: 'socket dropped' }, { _tag: 'Stale', reason: { _tag: 'Unknown' } }],
      [{ _tag: 'Stale', reason: { _tag: 'Resync', attempt: 2 } }, { _tag: 'Stale', reason: { _tag: 'Unknown' } }],
      [{ _tag: 'Stale', reason: { _tag: 'Evicted' } }, { _tag: 'Stale', reason: { _tag: 'Evicted' } }],
      [{ _tag: 'Unknown', detail: 'unavailable' }, { _tag: 'Stale', reason: { _tag: 'Unknown' } }],
    ]
    for (const [freshness, status] of cases)
      expect(syncStatusFromFreshness(freshness)).toEqual(status)
  })

  it('uses only SDK failure causes, never inventing Local for noncap failures', () => {
    expect(syncStatusFromFailure(new SubscriptionLimit({ cap: 4 }))).toEqual({
      _tag: 'Failed', cause: { _tag: 'Local', kind: 'subscription-limit', detail: { cap: 4 } },
    })
    expect(syncStatusFromFailure(new Rejected({ code: 'forbidden', message: 'no access' }))).toEqual({
      _tag: 'Failed', cause: { _tag: 'Server', code: 'forbidden', message: 'no access' },
    })
    for (const failure of [
      new Rejected({ code: undefined, message: 'no access' }),
      new Rejected({ code: '', message: 'no access' }),
      new AttachFailure({ message: 'attach failed' }),
    ]) expect(syncStatusFromFailure(failure)).toEqual({ _tag: 'Failed', cause: { _tag: 'Unknown' } })
    expect(syncStatusFromFailure(new AttachFailure({ code: 'forbidden', message: 'server attach refusal' }))).toEqual({
      _tag: 'Failed', cause: { _tag: 'Server', code: 'forbidden', message: 'server attach refusal' },
    })
  })
})
