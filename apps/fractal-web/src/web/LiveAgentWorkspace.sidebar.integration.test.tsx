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
import { SidebarAgentRow, SidebarTreeLevel } from '@smalltalk/fractal-ui/assistant-ui/shell'
import { within } from 'storybook/test'
import { sidebarRow } from './sidebarRow.ts'
import { fleetFromAgents } from '../data/projections.ts'

// Vitest hoists the factory above static imports; load the shared mock inside it.
vi.mock('@stylexjs/stylex', async () => ({
  ...await import('../testStylex.ts'),
  props: (...styles: unknown[]) => ({ 'data-style-contract': JSON.stringify(Object.assign({}, ...styles.filter(style => typeof style === 'object' && style !== null))) }),
}))

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
const sidebar = () => host.querySelector<HTMLElement>('aside[aria-label="Agents"]')!
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
  it('shows the live Connected dot, then connection wording instead of the raw gateway host while connecting', async () => {
    registry.set(agents, observed({ value: [agent] }))
    await mount(width)
    expect(roster().querySelectorAll('[data-testid="taste-agent-row"]')).toHaveLength(1)
    expect(roster().textContent).toContain(agent.name)
    expect(footerStatus().textContent).not.toContain(gateway)
    expect(footerStatus().textContent).toBe('Connected')
    expect(footerStatus().querySelector('[data-sidebar-connection-dot]')).not.toBeNull()
    await act(async () => registry.set(gatewaySync, { status: { _tag: 'Connecting', attempt: 1, since: 0 }, observedAt: 0 }))
    expect(footerStatus().textContent).toBe('Connecting · 5s')
    expect(footerStatus().querySelector('[data-sidebar-connection-dot]')).toBeNull()
    expect(sidebar().textContent).not.toContain(gateway)
  })

  it('replaces the Connected indicator with the offline notice when the network goes offline despite a live observation', async () => {
    const network = Atom.make({ _tag: 'Offline' } as const)
    source = { ...source, network }
    registry.set(agents, observed({ value: [agent] }))
    await mount(width)
    expect(footerStatus().textContent).toBe('You are offline')
    expect(footerStatus().querySelector('[data-sidebar-connection-dot]')).toBeNull()
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

describe('kit sidebar time placement', () => {
  it('reclaims the time track for nested titles while retaining time and the status glyph', async () => {
    const item = { ref: 'agent/example', id: 'agent/example', title: 'Example Agent', host: 'host/example', status: 'idle', statusLabel: 'Idle', freshness: 'live', children: [], usage: { _tag: 'Unknown' }, duration: { _tag: 'Unknown' }, lastTurn: { _tag: 'Known', at: 4000, kind: 'turn-completed' } } as const
    const mountRow = async (timePlacement: 'column' | 'subtitle', level = 1) => {
      await act(async () => root.render(<SidebarTreeLevel value={level}><SidebarAgentRow inTree item={item} now={5000} extraSignals={[]} timePlacement={timePlacement} /></SidebarTreeLevel>))
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
    // The 16px step spends title space exactly once. The browser stories measure
    // text/glyph/right-column positions; jsdom checks the actual emitted contract.
    for (const level of [1, 2, 3, 4, 5, 6]) {
      const nested = await mountRow('subtitle', level)
      const geometry = JSON.parse(nested.getAttribute('data-style-contract')!)
      expect(geometry.paddingInlineStart).toBe(`calc(${geometry.paddingInline} + ${level - 1} * var(--sidebar-tree-step, 16px))`)
    }
    expect(subtitle.querySelector('[data-row-column="time"]')).toBeNull()
    expect(subtitle.querySelector('[data-row-column="subtitle"] [data-row-field="last-turn"]')).not.toBeNull()
    expect(subtitle.querySelector('[data-row-column="status"]')).not.toBeNull()
    // Negative control: the unchanged/default layout cannot satisfy the reclaimed-track contract.
    expect(previousGrid).not.toContain('0px 0px max-content')
  })
})

describe('responsive agent navigation', () => {
  it.each([440, 700])('starts closed below 900px and opens a minimum-width modal at %ipx', async width => {
    registry.set(agents, observed({ value: [agent] }))
    await mount(width)
    expect(host.querySelector('aside[aria-label="Agents"]')).toBeNull()
    expect(host.querySelector('[data-resizer="live-agent-sidebar"]')).toBeNull()
    const opener = within(host).getByRole('button', { name: 'Expand sidebar' })
    await act(async () => opener.click())
    const dialog = within(host).getByRole('dialog', { name: 'Agent navigation' })
    const workspace = host.querySelector<HTMLElement>('section[aria-label="Agent workspace"]')!
    expect(dialog.getAttribute('aria-modal')).toBe('true')
    expect(workspace.closest('[inert], [aria-hidden="true"]')).not.toBeNull()
    expect(dialog.closest('[inert], [aria-hidden="true"]')).toBeNull()
    expect(sidebar().getAttribute('data-sidebar-presentation')).toBe('drawer')
    expect(Number.parseFloat(sidebar().style.width)).toBeGreaterThanOrEqual(240)
    await act(async () => within(host).getByRole('button', { name: 'Close agents' }).click())
    expect(host.querySelector('aside[aria-label="Agents"]')).toBeNull()
    expect(workspace.closest('[inert], [aria-hidden="true"]')).toBeNull()
    await act(async () => { document.dispatchEvent(new KeyboardEvent('keydown', { key: 'b', ctrlKey: true, bubbles: true })) })
    const reopened = within(host).getByRole('dialog', { name: 'Agent navigation' })
    expect(reopened.getAttribute('aria-modal')).toBe('true')
    expect(workspace.closest('[inert], [aria-hidden="true"]')).not.toBeNull()
  })

  it.each([900, 1440])('keeps a minimum-width dock and resize floor at %ipx', async width => {
    registry.set(agents, observed({ value: [agent] }))
    await mount(width)
    expect(sidebar().getAttribute('data-sidebar-presentation')).toBe('dock')
    expect(Number.parseFloat(sidebar().style.width)).toBeGreaterThanOrEqual(240)
    expect(within(host).getByRole('separator', { name: 'Agent sidebar width' }).getAttribute('aria-valuemin')).toBe('240')
    expect(host.querySelector('[role="dialog"]')).toBeNull()
  })
})
