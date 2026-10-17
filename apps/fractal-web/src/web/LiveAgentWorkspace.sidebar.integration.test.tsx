// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { Agent, decodeUnknownSync, type Attention, type Mission, type TerminalScreen } from '@smalltalk/st3-client/schema'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import * as Option from 'effect/Option'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { DataSourceProvider } from '../data/react.tsx'
import { fixtureSource } from '../data/fixtureSource.ts'
import { initialFeedSync, type FeedSyncObservation } from '../data/feedSync.ts'
import { observed, waiting, type ConversationPage, type DataSource, type Feed } from '../data/source.ts'
import { LiveAgentWorkspace } from './LiveAgentWorkspace.tsx'
import * as dataHooks from '../data/react.tsx'
import * as rowProjection from './sidebarRow.ts'

vi.mock('@stylexjs/stylex', () => import('../testStylex.ts'))

const gateway = 'gateway.example.invalid'
const agent = decodeUnknownSync(Agent)({ kind: 'agent', id: 'agent/example', name: 'Example Agent', host_id: 'host/example', runtime_ids: [], reachability: 'reachable', state: 'running', harness_state: 'idle', blocked_on: null, fault: null, revision: '1', updated_at: '2026-10-04T12:00:00.000Z' })
let registry: AtomRegistry.AtomRegistry
let root: Root
let host: HTMLDivElement
let agents: Atom.Writable<Feed<readonly Agent[]>>
let gatewaySync: Atom.Writable<FeedSyncObservation>
let source: DataSource

beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
  Object.defineProperty(Range.prototype, 'getBoundingClientRect', { configurable: true, value: () => new DOMRect() })
  Object.defineProperty(Range.prototype, 'getClientRects', { configurable: true, value: () => [] })
  // Keep this regression on the shell: no frame requests speculative transcript code.
  vi.stubGlobal('requestAnimationFrame', () => 1)
  vi.stubGlobal('cancelAnimationFrame', () => undefined)
  window.localStorage.clear()
  window.history.replaceState(null, '', '/?open=resource/example:detail')
  registry = AtomRegistry.make()
  agents = Atom.make<Feed<readonly Agent[]>>(waiting)
  gatewaySync = Atom.make<FeedSyncObservation>({ status: { _tag: 'Live', since: 0 }, observedAt: 0 })
  source = {
    ...fixtureSource({ world: { now: 5000, gateway, agents: [], missions: [], attention: [], events: [], conversations: {}, terminals: {}, envelopes: {}, usage: { _tag: 'undeclared' } } }),
    agents,
    sync: {
      gateway: gatewaySync,
      agents: Atom.make(initialFeedSync<readonly Agent[]>(0)),
      missions: Atom.make(initialFeedSync<readonly Mission[]>(0)),
      attention: Atom.make(initialFeedSync<readonly Attention[]>(0)),
      conversation: () => Atom.make(initialFeedSync<ConversationPage>(0)),
      terminal: () => Atom.make(initialFeedSync<TerminalScreen>(0)),
    },
  }
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
})

afterEach(async () => {
  await act(async () => root.unmount())
  registry.dispose()
  host.remove()
  vi.unstubAllGlobals()
  window.history.replaceState(null, '', '/')
})

const mount = async (width: number) => {
  Object.defineProperty(window, 'innerWidth', { configurable: true, value: width })
  await act(async () => root.render(<DataSourceProvider source={source} registry={registry}><LiveAgentWorkspace /></DataSourceProvider>))
}
const sidebar = () => host.querySelector('aside[aria-label="Agents"]')!
const roster = () => sidebar().querySelector('nav[aria-label="Agent roster"]')!
const footerStatus = () => sidebar().querySelector('footer [role="status"]')!

it('keeps workspace providers and unchanged roster rows out of the one-second clock render', async () => {
  const now = Atom.make(5000)
  source = { ...source, now }
  registry.set(agents, observed({ value: [agent, { ...agent, id: 'agent/other', name: 'Other Agent' }] }))
  const workspaceRenders = vi.spyOn(dataHooks, 'useFleet')
  const projectedRows = vi.spyOn(rowProjection, 'sidebarRow')
  try {
    await mount(1440)
    const workspaceCount = workspaceRenders.mock.calls.length
    const rowCount = projectedRows.mock.calls.length
    for (let second = 1; second <= 10; second += 1) {
      await act(async () => registry.set(now, 5000 + second * 1000))
    }
    expect(workspaceRenders).toHaveBeenCalledTimes(workspaceCount)
    expect(projectedRows).toHaveBeenCalledTimes(rowCount)
  } finally {
    workspaceRenders.mockRestore()
    projectedRows.mockRestore()
  }
})

it('refreshes sidebar activity labels at their exact second and minute boundaries', async () => {
  const now = Atom.make(5000)
  source = { ...source, now }
  const timedAgent = decodeUnknownSync(Agent)({ kind: 'agent', id: 'agent/example', name: 'Example Agent', host_id: 'host/example', runtime_ids: [], reachability: 'reachable', state: 'running', harness_state: 'idle', blocked_on: null, fault: null, revision: '1', updated_at: '1970-01-01T00:00:01.000Z', last_activity_at: '1970-01-01T00:00:01.000Z' })
  registry.set(agents, observed({ value: [timedAgent] }))
  await mount(1440)
  const time = () => roster().querySelector('[data-row-column="time"] time')!.textContent
  expect(time()).toBe('4s')
  await act(async () => registry.set(now, 6000))
  expect(time()).toBe('5s')
  await act(async () => registry.set(now, 60_000))
  expect(time()).toBe('59s')
  await act(async () => registry.set(now, 61_000))
  expect(time()).toBe('1m')
  await act(async () => registry.set(now, 120_000))
  expect(time()).toBe('1m')
  await act(async () => registry.set(now, 121_000))
  expect(time()).toBe('2m')
})

it('reveals a slightly future activity timestamp when only the leaf clock catches up', async () => {
  const now = Atom.make(5000)
  source = { ...source, now }
  const timedAgent = decodeUnknownSync(Agent)({ kind: 'agent', id: 'agent/example', name: 'Example Agent', host_id: 'host/example', runtime_ids: [], reachability: 'reachable', state: 'running', harness_state: 'idle', blocked_on: null, fault: null, revision: '1', updated_at: '1970-01-01T00:00:05.500Z', last_activity_at: '1970-01-01T00:00:05.500Z' })
  registry.set(agents, observed({ value: [timedAgent] }))
  const projectedRows = vi.spyOn(rowProjection, 'sidebarRow')
  try {
    await mount(1440)
    const count = projectedRows.mock.calls.length
    expect(roster().querySelector('[data-row-column="time"] time')).toBeNull()
    await act(async () => registry.set(now, 6000))
    expect(roster().querySelector('[data-row-column="time"] time')?.textContent).toBe('0s')
    expect(projectedRows).toHaveBeenCalledTimes(count)
  } finally {
    projectedRows.mockRestore()
  }
})

it('reveals a future status boundary in ClockedThreadHeader without rendering the header projection', async () => {
  const now = Atom.make(5000)
  source = { ...source, now }
  const at = '1970-01-01T00:00:05.500Z'
  const suspended = decodeUnknownSync(Agent)({ kind: 'agent', id: 'agent/example', name: 'Example Agent', host_id: 'host/example', runtime_ids: [], reachability: 'reachable', state: 'suspended', harness_state: 'idle', blocked_on: null, fault: null, revision: '1', updated_at: at, suspension: { action: 'suspend', blocking: [], operation_id: 'operation/example', phase: 'suspended', suspended_at: at, updated_at: at } })
  registry.set(agents, observed({ value: [suspended] }))
  window.history.replaceState(null, '', '/w/agent/example')
  const projectedRows = vi.spyOn(rowProjection, 'sidebarRow')
  try {
    await mount(1440)
    const header = host.querySelector('section[aria-label="Agent workspace"] header')!
    expect(header).not.toBeNull()
    const count = projectedRows.mock.calls.length
    expect(header.querySelector('time')).toBeNull()
    await act(async () => registry.set(now, 6000))
    expect(header.querySelector('time')?.textContent).toBe('0s')
    expect(projectedRows).toHaveBeenCalledTimes(count)
  } finally {
    projectedRows.mockRestore()
  }
})

// Both the real data derivation and the kit's SidebarAgentRow DOM participate.
describe.each([1280, 1440, 2980])('sidebar footer at %ipx', width => {
  it('dismisses the hovered row on navigation while preserving every roster node', async () => {
    const other: Agent = { ...agent, id: 'agent/other', name: 'Other Agent' }
    registry.set(agents, observed({ value: [agent, other] }))
    await mount(width)
    const before = [...roster().querySelectorAll('[data-testid="taste-agent-row"]')]
    const hovered = before[0]!.closest<HTMLElement>('[data-wf-agent-ref]')!
    const escape = vi.fn()
    hovered.addEventListener('keydown', escape)
    await act(async () => hovered.dispatchEvent(new MouseEvent('mouseover', { bubbles: true })))
    await act(async () => {
      window.history.pushState(null, '', '/w/agent/other?open=thread')
      window.dispatchEvent(new PopStateEvent('popstate'))
    })
    expect(escape).toHaveBeenCalledOnce()
    expect(escape.mock.calls[0]![0].key).toBe('Escape')
    const after = [...roster().querySelectorAll('[data-testid="taste-agent-row"]')]
    expect(after).toHaveLength(before.length)
    before.forEach((node, index) => expect(after[index]).toBe(node))
    expect(before.every(node => node.isConnected)).toBe(true)
    expect(roster().textContent).toContain(agent.name)
    expect(roster().textContent).toContain(other.name)
  })
  it('shows connection wording instead of the raw gateway host while live or connecting', async () => {
    registry.set(agents, observed({ value: [agent] }))
    await mount(width)
    expect(roster().querySelectorAll('[data-testid="taste-agent-row"]')).toHaveLength(1)
    expect(roster().textContent).toContain(agent.name)
    expect(footerStatus().textContent).not.toContain(gateway)
    expect(footerStatus().textContent).toBe('Connected')
    await act(async () => registry.set(gatewaySync, { status: { _tag: 'Connecting', attempt: 1, since: 0 }, observedAt: 0 }))
    expect(footerStatus().textContent).toBe('Connecting · 5s')
    expect(sidebar().textContent).not.toContain(gateway)
  })

  it('removes the waiting notice and skeleton as soon as roster rows are published', async () => {
    await mount(width)
    expect(sidebar().textContent).toContain('Waiting for the agent roster.')
    expect(roster().querySelectorAll('[data-testid="taste-agent-row"]')).toHaveLength(0)
    expect(roster().querySelector('[data-wf-roster-skeleton]')).not.toBeNull()
    await act(async () => registry.set(agents, { _tag: 'Observed', value: [agent], freshness: 'stale', coverage: { _tag: 'Partial' } }))
    expect(roster().querySelectorAll('[data-testid="taste-agent-row"]')).toHaveLength(1)
    expect(roster().textContent).toContain(agent.name)
    expect(sidebar().textContent).not.toContain('Waiting for the agent roster.')
    for (const node of host.querySelectorAll('[role="status"], [aria-live], [aria-label], [aria-description]')) {
      expect(node.textContent).not.toContain('Waiting for the agent roster.')
      expect(node.getAttribute('aria-label') ?? '').not.toContain('Waiting for the agent roster.')
      expect(node.getAttribute('aria-description') ?? '').not.toContain('Waiting for the agent roster.')
    }
    expect(roster().querySelector('[data-wf-roster-skeleton]')).toBeNull()
    expect(sidebar().textContent).toContain('Showing a partial roster · live updates pending')
    await act(async () => registry.set(agents, observed({ value: [agent] })))
    expect(sidebar().textContent).not.toContain('Waiting for the agent roster.')
    expect(sidebar().textContent).not.toContain('live updates pending')
    await act(async () => registry.set(agents, observed({ value: [] })))
    expect(roster().querySelectorAll('[data-testid="taste-agent-row"]')).toHaveLength(0)
    expect(sidebar().textContent).not.toContain('Waiting for the agent roster.')
  })
})

describe('shipped live sidebar machine query', () => {
  it('applies typed hosts to the real roster and preserves substring text and stripped highlights', async () => {
    const candidates: readonly Agent[] = [
      { ...agent, id: 'agent/compiler', name: 'Compiler Alpha', host_id: Option.some('host/synthetic-138/1') },
      { ...agent, id: 'agent/sidebar', name: 'Sidebar Beta', host_id: Option.some('host/SYNTHETIC-138/2') },
      // Text looks like a matching host but must not satisfy a machine facet.
      { ...agent, id: 'agent/decoy', name: 'Compiler synthetic-138/1 decoy', host_id: Option.some('host/laptop24') },
    ]
    registry.set(agents, observed({ value: candidates }))
    await mount(1440)
    const input = sidebar().querySelector<HTMLInputElement>('input')!
    expect(input.getAttribute('aria-description')).toMatch(/machine:.*host:/)
    const search = async (value: string) => {
      await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, value)
        input.dispatchEvent(new Event('input', { bubbles: true }))
      })
    }
    const names = () => [...roster().querySelectorAll('[data-testid="taste-agent-row"]')].map(row => row.textContent)
    expect(names()).toHaveLength(3)
    for (const [query, expected] of [
      ['machine:synthetic-138/1', ['Compiler Alpha']],
      ['machine:synthetic-138/1,synthetic-138/2', ['Compiler Alpha', 'Sidebar Beta']],
      ['HOST:SYNTHETIC-138', ['Compiler Alpha', 'Sidebar Beta']],
      ['machine:synthetic-138 host:synthetic-138/1', ['Compiler Alpha']],
      ['machine:synthetic-138/1 host:synthetic-138/2', []],
      ['machine:synthetic-138/9', []],
      ['machine: host:,,', ['Compiler Alpha', 'Sidebar Beta', 'Compiler synthetic-138/1 decoy']],
      ['Alpha machine:SYNTHETIC-138', ['Compiler Alpha']],
      ['cmp machine:synthetic-138', []],
      ['synthetic-138/1', ['Compiler Alpha', 'Compiler synthetic-138/1 decoy']],
      ['machine:host/synthetic-138/1', []],
    ] as const) {
      await search(query)
      expect(names()).toHaveLength(expected.length)
      expected.forEach(name => expect(roster().textContent).toContain(name))
    }
    await search('machine:synthetic-138/1')
    expect(roster().querySelectorAll('mark')).toHaveLength(0)
    await search('Alpha machine:synthetic-138/1')
    expect([...roster().querySelectorAll('mark')].map(mark => mark.textContent).join('')).toContain('Alpha')
    expect([...roster().querySelectorAll('mark')].some(mark => mark.textContent?.includes('machine:'))).toBe(false)
  })
})
