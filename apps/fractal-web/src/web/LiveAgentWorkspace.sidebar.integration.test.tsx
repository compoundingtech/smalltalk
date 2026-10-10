// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { Agent, decodeUnknownSync, type Attention, type Mission, type TerminalScreen } from '@smalltalk/st3-client/schema'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { DataSourceProvider } from '../data/react.tsx'
import { fixtureSource } from '../data/fixtureSource.ts'
import { initialFeedSync, type FeedSyncObservation } from '../data/feedSync.ts'
import { observed, waiting, type ConversationPage, type DataSource, type Feed } from '../data/source.ts'
import { LiveAgentWorkspace } from './LiveAgentWorkspace.tsx'
import { SidebarAgentRow } from '@smalltalk/fractal-ui/assistant-ui/shell'

vi.mock('@stylexjs/stylex', () => ({ create: (styles: unknown) => styles, defineVars: (variables: unknown) => variables, createTheme: () => ({}), keyframes: () => 'animation', props: (...styles: unknown[]) => ({ 'data-style-contract': JSON.stringify(Object.assign({}, ...styles.filter(style => typeof style === 'object' && style !== null))) }) }))

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

describe('kit sidebar time placement', () => {
  it('reclaims the time track for nested titles while retaining time and the status glyph', async () => {
    const item = { ref: 'agent/example', id: 'agent/example', title: 'Example Agent', host: 'host/example', status: 'idle', statusLabel: 'Idle', freshness: 'live', children: [], usage: { _tag: 'Unknown' }, duration: { _tag: 'Unknown' }, lastTurn: { _tag: 'Known', at: 4000, kind: 'turn-completed' } } as const
    const mountRow = async (timePlacement: 'column' | 'subtitle') => {
      await act(async () => root.render(<SidebarAgentRow inTree item={item} now={5000} extraSignals={[]} timePlacement={timePlacement} />))
      return host.querySelector('[data-testid="taste-agent-row"]')!
    }
    // jsdom cannot measure layout. Assert the real kit's emitted grid contract instead;
    // browser measurements cover the resulting pixels, including fonts and RAC padding.
    const column = await mountRow('column')
    const previousGrid = JSON.parse(column.getAttribute('data-style-contract')!).gridTemplateColumns
    expect(previousGrid).toContain('var(--sidebar-metric-track')
    expect(column.querySelector('[data-row-column="time"] [data-row-field="last-turn"]')).not.toBeNull()
    const subtitle = await mountRow('subtitle')
    const nextGrid = JSON.parse(subtitle.getAttribute('data-style-contract')!).gridTemplateColumns
    expect(nextGrid).toContain('0px 0px max-content')
    expect(nextGrid).not.toContain('var(--sidebar-metric-track')
    // Real 1440x900 browser fixture: the flat reference title at 256px is 170px.
    // Read the emitted kit geometry, not jsdom's (nonexistent) layout. RAC's live
    // tree adds no inset; the roster contributes 8px on each side and hierarchy 8px.
    const geometry = JSON.parse(subtitle.getAttribute('data-style-contract')!)
    const signals = JSON.parse(subtitle.querySelector('[data-row-trailing-signals]')!.getAttribute('data-style-contract')!)
    const px = (value: string) => Number.parseFloat(value)
    for (const level of [2, 3]) {
      const width = 256 - 16 - (level - 1) * 8 - 2 * px(geometry.paddingInline)
        - px(nextGrid) - px(signals.minWidth) - px(signals.marginInlineStart) - 3 * px(geometry.columnGap)
      expect(width, `level ${level} versus measured flat reference`).toBeGreaterThanOrEqual(170)
    }
    expect(subtitle.querySelector('[data-row-column="time"]')).toBeNull()
    expect(subtitle.querySelector('[data-row-column="subtitle"] [data-row-field="last-turn"]')).not.toBeNull()
    expect(subtitle.querySelector('[data-row-column="status"]')).not.toBeNull()
    // Negative control: the unchanged/default layout cannot satisfy the reclaimed-track contract.
    expect(previousGrid).not.toContain('0px 0px max-content')
  })
})
