// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { Agent, decodeUnknownSync, type AgentEncoded } from '@smalltalk/st3-client/schema'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { SidebarAgentRow } from '@smalltalk/fractal-ui/assistant-ui/shell'
import { DataSourceProvider, useFleet, useNow } from '../data/react.tsx'
import { fixtureSource } from '../data/fixtureSource.ts'
import { observed, type Feed } from '../data/source.ts'
import { sidebarRow } from './sidebarRow.ts'

vi.mock('@stylexjs/stylex', () => ({ create: (styles: unknown) => styles, defineVars: (variables: unknown) => variables, createTheme: () => ({}), keyframes: () => 'animation', props: () => ({}) }))
// jsdom has no Range layout; the kit row measures retained text with it.
Object.defineProperty(Range.prototype, 'getBoundingClientRect', { configurable: true, value: () => new DOMRect() })
Object.defineProperty(Range.prototype, 'getClientRects', { configurable: true, value: () => [] })
const makeRow = (change: Partial<AgentEncoded> = {}) => decodeUnknownSync(Agent)({
  kind: 'agent', id: 'agent/example', name: 'Example', host_id: 'host/example', runtime_ids: [],
  reachability: 'reachable', state: 'running', revision: '1', incarnation_id: 'first', updated_at: '1970-01-01T00:00:04.000Z',
  usage: { aggregation: 'cumulative-per-incarnation-else-response-deltas', incarnation_count: 1, input_tokens: 100, output_tokens: 200, cached_tokens: 300, total_tokens: 480_035_214, context: { used_tokens: 61_206, window_tokens: 1_000_000, used_percent: 6.1206, observed_at_unix_ms: 3000 } },
  ...change,
})
function ContextRows() {
  const fleet = useFleet(), now = useNow()
  if (fleet._tag !== 'Observed') return null
  return <div>{fleet.value.agents.map(agent => <SidebarAgentRow key={agent.ref} item={sidebarRow({ agent, stale: fleet.freshness === 'stale', now })} now={now} extraSignals={[]} onOpen={() => undefined} />)}</div>
}
let registry: AtomRegistry.AtomRegistry
let agents: Atom.Writable<Feed<readonly Agent[]>>
let host: HTMLDivElement
let root: Root
beforeEach(async () => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  vi.stubGlobal('ResizeObserver', class { observe() {} disconnect() {} })
  vi.stubGlobal('requestAnimationFrame', () => 1)
  vi.stubGlobal('cancelAnimationFrame', () => undefined)
  registry = AtomRegistry.make()
  agents = Atom.make(observed<readonly Agent[]>({ value: [makeRow()] }))
  const source = { ...fixtureSource({ world: { now: 10_000, agents: [], missions: [], attention: [], events: [], conversations: {}, terminals: {}, envelopes: {}, usage: { _tag: 'undeclared' } } }), agents }
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
  await act(async () => root.render(<DataSourceProvider source={source} registry={registry}><ContextRows /></DataSourceProvider>))
})
afterEach(async () => { await act(async () => root.unmount()); registry.dispose(); host.remove(); vi.unstubAllGlobals() })

describe('native roster to real kit context signal', () => {
  it('renders measured occupancy with full accessible numerator/window/as-of, not cumulative accounting', () => {
    const signal = host.querySelector('[data-row-field="context-percent"]')!
    expect(signal.textContent).toBe('Context 6%')
    // The kit carries occupancy in the row description, never a native label or tooltip on the signal.
    expect(signal.getAttribute('aria-label')).toBeNull()
    expect(signal.getAttribute('title')).toBeNull()
    const button = host.querySelector<HTMLButtonElement>('button[aria-describedby]')!
    const description = document.getElementById(button.getAttribute('aria-describedby')!)!.textContent
    expect(description).toContain('Reported main-session context: 6%; 61,206 used tokens; 1,000,000 token window; as of 1970-01-01T00:00:03.000Z')
    expect(description).not.toContain('480,035,214')
    // Negative control mutates the displayed value: the observable criterion must reject it.
    signal.textContent = 'Context 4800%'
    expect(() => expect(signal.textContent).toBe('Context 6%')).toThrow()
  })
  it('clears across restart until a newer occupancy arrives and identifies retained stale evidence', async () => {
    const originalRow = host.querySelector('[data-testid="taste-agent-row"]')!
    await act(async () => registry.set(agents, observed({ value: [makeRow({ incarnation_id: 'second' })] })))
    expect(host.querySelector('[data-row-field="context-percent"]')).toBeNull()
    expect(host.querySelector('[data-testid="taste-agent-row"]')).toBe(originalRow)
    const newer = makeRow({ incarnation_id: 'second', usage: { aggregation: 'cumulative-per-incarnation-else-response-deltas', incarnation_count: 1, input_tokens: 100, output_tokens: 200, cached_tokens: 300, total_tokens: 480_035_214, context: { used_tokens: 0, window_tokens: 1_000_000, used_percent: 0, observed_at_unix_ms: 5000 } } })
    await act(async () => registry.set(agents, observed({ value: [newer] })))
    expect(host.querySelector('[data-row-field="context-percent"]')!.textContent).toBe('Context 0%')
    await act(async () => registry.set(agents, observed({ value: [newer], freshness: 'stale' })))
    const staleButton = host.querySelector<HTMLButtonElement>('button[aria-describedby]')!
    expect(document.getElementById(staleButton.getAttribute('aria-describedby')!)!.textContent).toContain('Last observed main-session context: 0%')
  })
  it('omits missing occupancy even with known large cumulative tokens', async () => {
    await act(async () => registry.set(agents, observed({ value: [makeRow({ usage: { aggregation: 'cumulative-per-incarnation-else-response-deltas', incarnation_count: 1, input_tokens: 100, output_tokens: 200, cached_tokens: 300, total_tokens: 480_035_214 } })] })))
    expect(host.querySelector('[data-row-field="context-percent"]')).toBeNull()
    expect(host.textContent).not.toContain('Context')
  })
})
