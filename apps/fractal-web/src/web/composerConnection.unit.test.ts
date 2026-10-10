import { describe, expect, it, vi } from 'vitest'
import { composerConnectionProps } from './composerConnection.ts'
import type { FeedSyncObservation } from '../data/feedSync.ts'

const reconnecting: FeedSyncObservation = {
  status: { _tag: 'Stale', reason: { _tag: 'Reconnecting', attempt: 2, nextAt: 9000, issue: 'Unrendered diagnostic' } },
  observedAt: 1000,
}

describe('composer connection props', () => {
  it('reports browser offline immediately without claiming the host is offline', () => {
    const reconnect = vi.fn()
    const props = composerConnectionProps({ network: { _tag: 'Offline' }, observation: reconnecting, now: 1000, gateway: 'alpha.example', reconnect })
    expect(props.connectionNotice).toMatchObject({ tone: 'offline', text: 'You are offline', action: { label: 'Reconnect now' } })
    expect(props.connectionNotice?.text).not.toContain('alpha.example is offline')
    props.connectionNotice?.action?.onPress()
    expect(reconnect).toHaveBeenCalledTimes(1)
  })
  it('uses the reconnect grace without rendering raw diagnostic detail', () => {
    expect(composerConnectionProps({ network: { _tag: 'Online' }, observation: reconnecting, now: 2999 })).toEqual({})
    expect(composerConnectionProps({ network: { _tag: 'Online' }, observation: reconnecting, now: 3000, gateway: 'alpha.example' })).toEqual({
      connectionNotice: { tone: 'reconnecting', text: 'alpha.example connection lost · reconnecting' },
    })
  })
  it('has no notice after a decoded live observation', () => {
    expect(composerConnectionProps({ network: { _tag: 'Online' }, observation: { status: { _tag: 'Live', since: 1000 }, observedAt: 1000 }, now: 3000 })).toEqual({})
  })
})
