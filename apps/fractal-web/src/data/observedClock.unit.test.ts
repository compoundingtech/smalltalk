import type { ConnectionState } from '@st3/sdk/effect'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { describe, expect, it } from 'vitest'
import type { FeedSyncObservation } from './feedSync.ts'
import { createObservedClock } from './observedClock.ts'
import type { NetworkReachability } from './source.ts'

const fixture = () => {
  const registry = AtomRegistry.make()
  const now = Atom.make(10000)
  const connection = Atom.make<ConnectionState>({ _tag: 'Live' })
  const network = Atom.make<NetworkReachability>({ _tag: 'Online' })
  const gateway = Atom.make<FeedSyncObservation>({ status: { _tag: 'Live', since: 1000 }, observedAt: 1000 })
  const clock = createObservedClock({ now, connection, network, sync: { gateway } })
  const unmount = registry.mount(clock)
  return { registry, now, connection, network, gateway, clock, dispose: () => { unmount(); registry.dispose() } }
}

describe('shared last-live presentation clock', () => {
  it('freezes on browser offline, retains the same frozen sample through ticks, and resumes after recovery', () => {
    const f = fixture()
    try {
      expect(f.registry.get(f.clock)).toEqual({ _tag: 'Live', now: 10000 })
      f.registry.set(f.now, 11000)
      expect(f.registry.get(f.clock)).toEqual({ _tag: 'Live', now: 11000 })
      f.registry.set(f.network, { _tag: 'Offline' })
      const paused = f.registry.get(f.clock)
      expect(paused).toEqual({ _tag: 'Paused', now: 11000 })
      for (const now of [12000, 30000, 60000]) {
        f.registry.set(f.now, now)
        expect(f.registry.get(f.clock)).toBe(paused)
      }
      f.registry.set(f.network, { _tag: 'Online' })
      expect(f.registry.get(f.clock)).toEqual({ _tag: 'Live', now: 60000 })
      f.registry.set(f.now, 61000)
      expect(f.registry.get(f.clock)).toEqual({ _tag: 'Live', now: 61000 })
    } finally { f.dispose() }
  })
  it('does not resume on an open socket before a decoded gateway observation becomes Live', () => {
    const f = fixture()
    try {
      expect(f.registry.get(f.clock).now).toBe(10000)
      f.registry.set(f.gateway, { status: { _tag: 'Stale', reason: { _tag: 'Unknown' } }, observedAt: 11000 })
      f.registry.set(f.now, 15000)
      expect(f.registry.get(f.clock)).toEqual({ _tag: 'Paused', now: 10000 })
      f.registry.set(f.gateway, { status: { _tag: 'Requested', since: 15000 }, observedAt: 15000 })
      expect(f.registry.get(f.clock)._tag).toBe('Paused')
      f.registry.set(f.gateway, { status: { _tag: 'Live', since: 16000 }, observedAt: 16000 })
      expect(f.registry.get(f.clock)).toEqual({ _tag: 'Live', now: 15000 })
    } finally { f.dispose() }
  })
  it('freezes an initially disconnected clock without manufacturing a live observation', () => {
    const f = fixture()
    try {
      f.registry.set(f.network, { _tag: 'Offline' })
      const clock = createObservedClock({ now: f.now, connection: f.connection, network: f.network })
      expect(f.registry.get(clock)).toEqual({ _tag: 'Paused', now: 10000 })
      f.registry.set(f.now, 20000)
      expect(f.registry.get(clock)).toEqual({ _tag: 'Paused', now: 10000 })
    } finally { f.dispose() }
  })
})
