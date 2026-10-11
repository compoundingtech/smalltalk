/** Per-follow freshness fold: derived only from transport events the SDK actually observes. */
import { describe, expect, it } from 'vitest'

import { initialFreshness, transitionFreshness } from './freshness.ts'

const requested = () => transitionFreshness(initialFreshness(), { _tag: 'SubscribeSent' }, 100)
const live = () => transitionFreshness(requested(), { _tag: 'Frame' }, 150)

describe('follow freshness fold', () => {
  it('starts Unknown and reaches Live only through a subscribe send and a real frame', () => {
    let state = initialFreshness()
    expect(state._tag).toBe('Unknown')
    state = transitionFreshness(state, { _tag: 'SubscribeSent' }, 100)
    expect(state).toEqual({ _tag: 'Requested', since: 100 })
    state = transitionFreshness(state, { _tag: 'Frame' }, 150)
    expect(state).toEqual({ _tag: 'Live', since: 150 })
    // Further frames carry no new verdict: silence is never re-confirmed (no heartbeat yet).
    expect(transitionFreshness(state, { _tag: 'Frame' }, 200)).toBe(state)
  })

  it('never reports Live from a closed socket: a drop is Reconnecting until a real frame', () => {
    let state = live()
    state = transitionFreshness(state, { _tag: 'SocketDropped', attempt: 1, issue: 'closed' }, 200)
    expect(state).toEqual({ _tag: 'Reconnecting', attempt: 1, issue: 'closed' })
    const refined = transitionFreshness(state, { _tag: 'SocketDropped', attempt: 2, issue: 'probe failed' }, 300)
    expect(refined).toEqual({ _tag: 'Reconnecting', attempt: 2, issue: 'probe failed' })
    // A resubscribe send goes back to Requested, never straight to Live.
    expect(transitionFreshness(refined, { _tag: 'SubscribeSent' }, 310)).toEqual({
      _tag: 'Requested',
      since: 310,
    })
    expect(transitionFreshness(refined, { _tag: 'Frame' }, 320)).toEqual({ _tag: 'Live', since: 320 })
  })

  it('counts consecutive resync attempts and resets the count on a live frame', () => {
    let state = transitionFreshness(live(), { _tag: 'Resync' }, 200)
    expect(state).toEqual({ _tag: 'Stale', reason: { _tag: 'Resync', attempt: 1 } })
    state = transitionFreshness(state, { _tag: 'Resync', code: 'store-rewound', message: 'rereading' }, 250)
    expect(state).toEqual({
      _tag: 'Stale',
      reason: { _tag: 'Resync', code: 'store-rewound', message: 'rereading', attempt: 2 },
    })
    state = transitionFreshness(state, { _tag: 'Frame' }, 300)
    expect(state).toEqual({ _tag: 'Live', since: 300 })
    state = transitionFreshness(state, { _tag: 'Resync' }, 350)
    expect(state).toEqual({ _tag: 'Stale', reason: { _tag: 'Resync', attempt: 1 } })
  })

  it('maps a transient transport error onto a reason-carrying Stale, never onto Live', () => {
    const state = transitionFreshness(live(), { _tag: 'Retry', code: 'overloaded', message: 'busy' }, 200)
    expect(state).toEqual({
      _tag: 'Stale',
      reason: { _tag: 'Resync', code: 'overloaded', message: 'busy', attempt: 1 },
    })
  })

  it('ends with Stale(Evicted) from any prior state', () => {
    const states = [initialFreshness(), requested(), live(), transitionFreshness(live(), { _tag: 'Resync' }, 200)]
    for (const state of states)
      expect(transitionFreshness(state, { _tag: 'Evicted' }, 999)).toEqual({
        _tag: 'Stale',
        reason: { _tag: 'Evicted' },
      })
  })
})
