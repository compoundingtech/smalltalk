// @vitest-environment jsdom
import * as React from 'react'
import { flushSync } from 'react-dom'
import { createRoot } from 'react-dom/client'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import type { ConnectionState } from '@st3/sdk/effect'
import { createSystemClock, useClockLabel, type ClockHost } from '@smalltalk/fractal-ui/human-time/clock'
import { formatDuration, formatRelative, nextDurationChange, nextRelativeChange } from '@smalltalk/fractal-ui/human-time/format'
import { expect, it } from 'vitest'
import { fixtureSource } from './fixtureSource.ts'
import { DataSourceProvider, useNow, useObservedClockState, useObservedNow } from './react.tsx'
import { minuteClock, type DataSource, type NetworkReachability } from './source.ts'

// HumanTime's labels without its StyleX shell (vitest does not compile StyleX): same text, same deadlines.
const Relative = ({ at, testId }: { readonly at: number; readonly testId: string }) =>
  <time data-testid={testId}>{useClockLabel((now, zone) => formatRelative(at, now, zone), now => nextRelativeChange(at, now))}</time>
const Elapsed = ({ at, testId }: { readonly at: number; readonly testId: string }) =>
  <time data-testid={testId}>{useClockLabel(now => formatDuration(now - at, { maxUnits: 1 }), now => nextDurationChange(at, now, { maxUnits: 1 }))}</time>

const emptyWorld = { now: 0, agents: [], missions: [], attention: [], events: [], conversations: {}, terminals: {}, envelopes: {}, usage: { _tag: 'undeclared' } } as const

/** A page host whose timers and visibility the test drives; every timer callback commits synchronously. */
const pageHost = (start: number) => {
  let time = start, hidden = false, nextId = 0, fired = 0
  const timers = new Map<number, { readonly at: number; readonly run: () => void }>()
  const listeners = new Set<() => void>()
  const host: ClockHost = {
    now: () => time,
    setTimer: (run, ms) => { timers.set(++nextId, { at: time + ms, run }); return nextId },
    clearTimer: timer => { timers.delete(timer as number) },
    hidden: () => hidden,
    onVisibilityChange: listener => { listeners.add(listener); return () => { listeners.delete(listener) } },
  }
  return {
    host,
    armed: () => timers.size,
    fired: () => fired,
    /** Advance wall time to `to`, firing due timers in order. */
    advance: (to: number) => {
      for (;;) {
        const [id, timer] = [...timers].sort(([, a], [, b]) => a.at - b.at)[0] ?? []
        if (id === undefined || timer === undefined || timer.at > to) break
        timers.delete(id)
        time = timer.at
        fired++
        flushSync(timer.run)
      }
      time = to
    },
    setHidden: (value: boolean) => { hidden = value; for (const listener of [...listeners]) flushSync(listener) },
  }
}

const mount = (source: DataSource, registry: AtomRegistry.AtomRegistry, node: React.ReactNode) => {
  const container = document.createElement('div')
  const root = createRoot(container)
  flushSync(() => root.render(<DataSourceProvider source={source} registry={registry}>{node}</DataSourceProvider>))
  return { container, unmount: () => { flushSync(() => root.unmount()); registry.dispose() } }
}

it('follows a fixture now atom in the leaf and still publishes pause/resume to the shell', () => {
  const registry = AtomRegistry.make()
  const now = Atom.make(0)
  const connection = Atom.make<ConnectionState>({ _tag: 'Live' })
  const network = Atom.make<NetworkReachability>({ _tag: 'Online' })
  const source = { ...fixtureSource({ world: emptyWorld }), now, connection, network }
  let shellRenders = 0, leafRenders = 0
  const Leaf = () => { leafRenders++; return <time>{useObservedNow().now}</time> }
  const Shell = () => { shellRenders++; const state = useObservedClockState(); return <section data-state={state}><Leaf /></section> }
  const page = mount(source, registry, <Shell />)
  try {
    for (let tick = 1; tick <= 600; tick++) flushSync(() => registry.set(now, tick * 1000))
    expect(shellRenders).toBe(1)
    expect(leafRenders).toBe(601)
    expect(page.container.querySelector('time')?.textContent).toBe('600000')
    flushSync(() => registry.set(network, { _tag: 'Offline' }))
    expect(page.container.querySelector('section')?.dataset.state).toBe('Paused')
    expect(shellRenders).toBe(2)
    flushSync(() => registry.set(now, 700_000))
    expect(page.container.querySelector('time')?.textContent).toBe('600000')
    flushSync(() => registry.set(network, { _tag: 'Online' }))
    expect(shellRenders).toBe(3)
    expect(page.container.querySelector('time')?.textContent).toBe('700000')
  } finally { page.unmount() }
})

it('wakes relative labels once per minute boundary and never re-renders their shell', () => {
  const start = Date.UTC(2026, 9, 13, 12, 0, 0)
  const page = pageHost(start)
  const registry = AtomRegistry.make()
  const source: DataSource = { ...fixtureSource({ world: { ...emptyWorld, now: start } }), clock: createSystemClock(page.host) }
  let shellRenders = 0
  const Shell = () => { shellRenders++; return <p><Relative at={start - 15_000} testId="relative" /><Elapsed at={start} testId="elapsed" /></p> }
  const view = mount(source, registry, <Shell />)
  const text = (id: string) => view.container.querySelector(`[data-testid="${id}"]`)?.textContent
  try {
    expect(text('relative')).toBe('just now')
    expect(text('elapsed')).toBe('0s')
    page.advance(start + 44_999)
    expect(text('relative')).toBe('just now')
    expect(text('elapsed')).toBe('44s')
    page.advance(start + 45_000)
    expect(text('relative')).toBe('1m ago')
    page.advance(start + 59_999)
    expect(text('elapsed')).toBe('59s')
    page.advance(start + 60_000)
    expect(text('elapsed')).toBe('1m')
    // Past the first minute both labels are minute-granular: ten minutes wake at most twice a minute.
    const before = page.fired()
    page.advance(start + 11 * 60_000)
    expect(page.fired() - before).toBeLessThanOrEqual(20)
    expect(text('relative')).toBe('11m ago')
    expect(text('elapsed')).toBe('11m')
    expect(shellRenders).toBe(1)
    expect(page.armed()).toBe(1)
  } finally { view.unmount() }
})

it('arms nothing while hidden and catches every label up once when visible again', () => {
  const start = Date.UTC(2026, 9, 13, 12, 0, 0)
  const page = pageHost(start)
  const registry = AtomRegistry.make()
  const source: DataSource = { ...fixtureSource({ world: { ...emptyWorld, now: start } }), clock: createSystemClock(page.host) }
  let rowRenders = 0
  const Row = () => {
    rowRenders++
    const now = useNow(now => nextDurationChange(start - 90_000, now, { maxUnits: 1 }))
    return <span data-testid="row">{Math.floor((now - start + 90_000) / 60_000)}m</span>
  }
  const view = mount(source, registry, <><Relative at={start - 5 * 60_000} testId="relative" /><Row /></>)
  const text = (id: string) => view.container.querySelector(`[data-testid="${id}"]`)?.textContent
  try {
    expect(text('relative')).toBe('5m ago')
    expect(text('row')).toBe('1m')
    page.setHidden(true)
    expect(page.armed()).toBe(0)
    const fired = page.fired(), renders = rowRenders
    page.advance(start + 30 * 60_000)
    expect(page.fired()).toBe(fired)
    expect(rowRenders).toBe(renders)
    page.setHidden(false)
    expect(text('relative')).toBe('35m ago')
    expect(text('row')).toBe('31m')
    expect(rowRenders).toBe(renders + 1)
    expect(page.armed()).toBe(1)
    page.advance(start + 30 * 60_000 + 30_000)
    expect(text('row')).toBe('32m')
    expect(text('relative')).toBe('35m ago')
    page.advance(start + 31 * 60_000)
    expect(text('relative')).toBe('36m ago')
  } finally { view.unmount() }
})

it('publishes the live minute clock once per wall minute through the real deadline scheduler', () => {
  const start = Date.UTC(2026, 9, 13, 12, 0, 20)
  const page = pageHost(start)
  const registry = AtomRegistry.make()
  const wall = minuteClock(createSystemClock(page.host))
  const seen: number[] = []
  const stop = registry.subscribe(wall, value => { seen.push(value) }, { immediate: true })
  try {
    expect(seen).toEqual([start])
    page.advance(start + 39_999)
    expect(seen).toEqual([start])
    // Every wall minute boundary dispatches exactly once, and the next minute is armed after it.
    page.advance(Date.UTC(2026, 9, 13, 12, 3, 30))
    expect(seen).toEqual([start, Date.UTC(2026, 9, 13, 12, 1), Date.UTC(2026, 9, 13, 12, 2), Date.UTC(2026, 9, 13, 12, 3)])
    expect(page.armed()).toBe(1)
    // Hidden: nothing armed or published; visible: one catch-up, then minutes resume.
    page.setHidden(true)
    page.advance(Date.UTC(2026, 9, 13, 12, 10, 15))
    expect(seen).toHaveLength(4)
    page.setHidden(false)
    expect(seen.at(-1)).toBe(Date.UTC(2026, 9, 13, 12, 10, 15))
    page.advance(Date.UTC(2026, 9, 13, 12, 11))
    expect(seen.at(-1)).toBe(Date.UTC(2026, 9, 13, 12, 11))
    expect(seen).toHaveLength(6)
  } finally { stop(); registry.dispose() }
  expect(page.armed()).toBe(0)
})
