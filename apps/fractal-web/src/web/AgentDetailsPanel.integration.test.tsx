// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { Agent, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { within } from 'storybook/test'
import { DataSourceProvider } from '../data/react.tsx'
import { fixtureSource } from '../data/fixtureSource.ts'
import { observed } from '../data/source.ts'
import type { Feed } from '../data/source.ts'
import { AgentDetailsPanel } from './AgentDetailsPanel.tsx'

vi.mock('@stylexjs/stylex', () => import('../testStylex.ts'))
const row = decodeUnknownSync(Agent)({
  kind: 'agent', id: 'agent/synthetic', name: 'Synthetic Details seat', host_id: 'host/synthetic', runtime_ids: [],
  reachability: 'reachable', state: 'running', revision: '1', incarnation_id: 'first', updated_at: '1970-01-01T00:00:04.000Z',
  usage: { aggregation: 'cumulative-per-incarnation-else-response-deltas', incarnation_count: 1, input_tokens: 100, output_tokens: 200, cached_tokens: 300, total_tokens: 480_035_214, context: { used_tokens: 61_206, window_tokens: 1_000_000, used_percent: 6.1206, observed_at_unix_ms: 3000, model: 'synthetic-reported-model' } },
})
let registry: AtomRegistry.AtomRegistry
let root: Root
let host: HTMLDivElement
let agents: Atom.Writable<Feed<readonly Agent[]>>
const onClose = vi.fn()
beforeEach(async () => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
  vi.stubGlobal('requestAnimationFrame', () => 1)
  vi.stubGlobal('cancelAnimationFrame', () => undefined)
  Object.defineProperty(Range.prototype, 'getBoundingClientRect', { configurable: true, value: () => new DOMRect() })
  Object.defineProperty(Range.prototype, 'getClientRects', { configurable: true, value: () => [] })
  window.localStorage.clear()
  registry = AtomRegistry.make()
  agents = Atom.make<Feed<readonly Agent[]>>(observed({ value: [row] }))
  const source = { ...fixtureSource({ world: { now: 10_000, agents: [], missions: [], attention: [], events: [], conversations: {}, terminals: {}, envelopes: {}, usage: { _tag: 'undeclared' } } }), agents }
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
  onClose.mockClear()
  await act(async () => root.render(<DataSourceProvider source={source} registry={registry}><AgentDetailsPanel agentRef="agent/synthetic" width={320} scheme="dark" onClose={onClose} onOpen={() => undefined} /></DataSourceProvider>))
})
afterEach(async () => { await act(async () => root.unmount()); registry.dispose(); host.remove(); vi.unstubAllGlobals() })

describe('native observations mounted on the production kit Details pane', () => {
  it('shows real native identity/context and actual header controls without fixture fallback', () => {
    expect(host.querySelector('[data-testid="details-pane"]')).not.toBeNull()
    expect(host.textContent).toContain('Synthetic Details seat')
    expect(host.textContent).toContain('synthetic-reported-model')
    expect(within(host).getByRole('tab', { name: 'Details' }).getAttribute('aria-selected')).toBe('true')
    expect(within(host).getByRole('button', { name: 'Visible sections' })).not.toBeNull()
    const context = within(host).getByRole('progressbar', { name: 'Context window' })
    expect(Number(context.getAttribute('aria-valuenow'))).toBeCloseTo(6.1206, 6)
    expect(host.textContent).not.toMatch(/worker-1|Small Talk release|Self vs descendants.*\$0/)
    context.setAttribute('aria-valuenow', String(480_035_214 / 1_000_000 * 100))
    expect(() => expect(Number(context.getAttribute('aria-valuenow'))).toBeCloseTo(6.1206, 6)).toThrow()
  })
  it('hides not-supplied resource sections and names their absence once, without inventing empty counts', () => {
    for (const name of ['Pull requests', 'CI runs', 'Reviews', 'Files touched', 'Terminals', 'Missions / queue', 'History'])
      expect(within(host).queryByRole('region', { name })).toBeNull()
    expect(host.querySelector('[data-details-unreported-sections]')?.textContent).toContain('Not supplied:')
    expect(host.querySelector('[data-details-unreported-sections]')?.textContent).toContain('CI runs')
    expect(host.textContent).not.toContain('CI runs observations are not supplied')
    expect(within(host).getByRole('region', { name: 'Attention' }).textContent).toContain('No attention action observed')
  })
  it('preserves Changes as a selectable real captured-changes surface and returns to Details', async () => {
    await act(async () => within(host).getByRole('tab', { name: 'Changes' }).click())
    expect(host.querySelector('aside[aria-label="Changes"]')).not.toBeNull()
    expect(host.querySelector('[data-testid="details-pane"]')).toBeNull()
    expect(host.textContent).toContain('Captured changes unavailable')
    await act(async () => within(host).getByRole('tab', { name: 'Details' }).click())
    expect(host.querySelector('[data-testid="details-pane"]')).not.toBeNull()
    expect(host.querySelector('aside[aria-label="Changes"]')).toBeNull()
  })
  it('keeps the app source registry when kit tab state has its own registry and closes through the host', async () => {
    await act(async () => registry.set(agents, observed({ value: [{ ...row, name: 'Updated native seat' }] })))
    expect(host.textContent).toContain('Updated native seat')
    expect(host.textContent).not.toContain('Synthetic Details seat')
    await act(async () => within(host).getByRole('button', { name: 'Close agent inspector' }).click())
    expect(onClose).toHaveBeenCalledOnce()
  })
})
