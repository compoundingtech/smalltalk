import { describe, expect, it } from 'vitest'
import * as Option from 'effect/Option'

import {
  initialFeedSync,
  observeFeedSync,
  transitionFeedSync,
} from './feedSync.ts'

describe('feed sync observations', () => {
  it('keeps the client-observed last value across stale and failed transitions', () => {
    const initial = initialFeedSync<readonly string[]>(100)
    expect(Option.isNone(initial.last)).toBe(true)

    const requested = transitionFeedSync(initial, { _tag: 'Requested' }, 120)
    const live = observeFeedSync(requested, ['row'], 150)
    const stale = transitionFeedSync(live, {
      _tag: 'Stale',
      code: 'resync-required',
      message: 'snapshot required',
    }, 170)
    const failed = transitionFeedSync(stale, {
      _tag: 'Failed',
      failure: { _tag: 'ConnectionRejected', message: 'permission denied' },
    }, 190)

    expect(Option.getOrUndefined(failed.last)).toEqual({ value: ['row'], observedAt: 150 })
    expect(failed.sync).toEqual({
      status: { _tag: 'Failed', failure: { _tag: 'ConnectionRejected', message: 'permission denied' } },
      observedAt: 190,
    })
  })

  it('changes transition time only when sync state changes, while every observation updates last time', () => {
    const connecting = initialFeedSync<string>(100)
    expect(transitionFeedSync(connecting, { _tag: 'Connecting', attempt: 1 }, 125)).toBe(connecting)

    const requested = transitionFeedSync(connecting, { _tag: 'Requested' }, 130)
    const repeated = transitionFeedSync(requested, { _tag: 'Requested' }, 160)
    expect(repeated).toBe(requested)

    const live = observeFeedSync(repeated, 'first', 180)
    const next = observeFeedSync(live, 'second', 220)
    expect(next.sync.observedAt).toBe(180)
    expect(Option.getOrUndefined(next.last)).toEqual({ value: 'second', observedAt: 220 })
  })

  it('keeps stale detail absent when the SDK has no reason to report', () => {
    const live = observeFeedSync(initialFeedSync<string>(100), 'row', 120)
    const stale = transitionFeedSync(live, { _tag: 'Stale' }, 130)
    expect(stale.sync.status).toEqual({ _tag: 'Stale' })
    expect(Option.getOrUndefined(stale.last)).toEqual({ value: 'row', observedAt: 120 })
  })
})
