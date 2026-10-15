import { describe, expect, it } from 'vitest'

import type { Feed } from '../data/source.ts'
import { inboxEmptyText } from './inboxEmpty.ts'

const none = [] as const
type Alerts = Feed<readonly never[]>

describe('inboxEmptyText', () => {
  it('says nothing needs the person only from a live read', () => {
    const live: Alerts = { _tag: 'Observed', value: none, freshness: 'live' }
    expect(inboxEmptyText(live)).toBe('Nothing needs you')
  })

  it('does not call an empty list good news before the alerts are loaded', () => {
    expect(inboxEmptyText({ _tag: 'Waiting' })).toBe('Checking for alerts…')
  })

  it('says why the alerts are missing when they could not be read', () => {
    const unavailable: Alerts = { _tag: 'Unavailable', reason: 'failed', detail: 'the gateway did not answer' }
    expect(inboxEmptyText(unavailable)).toBe('Alerts could not be loaded: the gateway did not answer')
  })

  it('says the view is out of date when the last read is stale or its refresh failed', () => {
    const stale: Alerts = { _tag: 'Observed', value: none, freshness: 'stale' }
    expect(inboxEmptyText(stale)).toBe('No alerts known — this view is out of date')
    const failed: Alerts = {
      _tag: 'Observed',
      value: none,
      freshness: 'live',
      error: { reason: 'failed', detail: 'refresh failed' },
    }
    expect(inboxEmptyText(failed)).toBe('No alerts known — this view is out of date')
  })
})
