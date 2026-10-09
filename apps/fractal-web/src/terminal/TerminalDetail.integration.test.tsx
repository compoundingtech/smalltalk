// @vitest-environment jsdom
import { Agent, decodeUnknownSync, type TerminalScreen } from '@smalltalk/st3-client/schema'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import * as React from 'react'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { fixtureSource } from '../data/fixtureSource.ts'
import { DataSourceProvider } from '../data/react.tsx'
import { observed, unavailable, waiting, type Feed } from '../data/source.ts'
import { makeScreen } from './fixtures.ts'
import { TerminalDetail } from './TerminalDetail.tsx'

vi.mock('@stylexjs/stylex', () => ({ create: (styles: unknown) => styles, defineVars: (variables: unknown) => variables, createTheme: () => ({}), keyframes: () => 'animation', props: () => ({}) }))

const ref = 'terminal/example'
const screen: TerminalScreen = { ...makeScreen({ scene: 'F3', columns: 40, rows: 4, frame: 0 }), terminal_id: ref }
const agent = (reachability: 'reachable' | 'unreachable') => decodeUnknownSync(Agent)({ kind: 'agent', id: 'agent/example', name: 'Example Agent', host_id: 'host/example', runtime_ids: [], reachability, state: 'running', harness_state: 'idle', blocked_on: null, fault: null, revision: '1', updated_at: '2026-10-04T12:00:00.000Z' })
let registry: AtomRegistry.AtomRegistry
let root: Root
let host: HTMLDivElement
beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
  // jsdom has no font loading; the registered terminal faces resolve as loaded.
  Object.defineProperty(document, 'fonts', { configurable: true, value: ['Wf Terminal Mono', 'Wf Terminal Nerd Mono'].map((family) => ({ family, load: async () => [] })) })
  registry = AtomRegistry.make()
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
})
afterEach(async () => {
  await act(async () => root.unmount())
  registry.dispose()
  host.remove()
  vi.unstubAllGlobals()
})
const mount = async ({ terminal, reachability = 'reachable' }: { readonly terminal: Feed<TerminalScreen>; readonly reachability?: 'reachable' | 'unreachable' }) => {
  const source = fixtureSource({ world: { now: 0, agents: [agent(reachability)], missions: [], attention: [], events: [], conversations: {}, terminals: {}, envelopes: {}, usage: { _tag: 'undeclared' } }, overrides: { terminal: { [ref]: terminal } } })
  await act(async () => root.render(<DataSourceProvider source={source} registry={registry}><React.Suspense fallback={<p>suspended</p>}><TerminalDetail address={{ ref, presentation: 'detail' }} visibility="visible" /></React.Suspense></DataSourceProvider>))
}

describe('terminal detail', () => {
  it.each([
    ['loading', waiting, 'Loading terminal…'],
    ['ungranted', unavailable({ reason: 'ungranted', detail: 'forbidden: raw gateway refusal' }), 'This terminal is not available to this view.'],
    ['unsupported', unavailable({ reason: 'unsupported', detail: 'raw capability detail' }), 'This gateway does not provide terminals.'],
    ['failed', unavailable({ reason: 'failed', detail: 'agent/example has no terminal runtime' }), 'The terminal could not be opened. Return to the thread and open it again.'],
  ] as const)('states %s with fixed copy and no transport detail', async (state, terminal, copy) => {
    await mount({ terminal })
    const status = host.querySelector('[data-terminal-state]')
    expect(status?.getAttribute('data-terminal-state')).toBe(state)
    expect(host.textContent).toBe(copy)
    expect(host.textContent).not.toMatch(/raw|forbidden|runtime|Unknown/)
  })

  it('renders the live grid with input gated exactly as the source publishes it and history honestly unavailable', async () => {
    await mount({ terminal: observed({ value: screen }) })
    const grid = host.querySelector(`[aria-label="Terminal: ${screen.title}"]`)
    expect(grid?.getAttribute('data-terminal-renderer')).toBe('T1')
    expect(grid?.querySelectorAll('[data-terminal-row]')).toHaveLength(screen.lines.length)
    expect(grid?.textContent).toContain(screen.lines.find((line) => line.text.trim() !== '')?.text.trim())
    expect(host.querySelector('[aria-label="Live"]')).not.toBeNull()
    expect(host.querySelector('textarea')?.disabled).toBe(true)
    expect(host.textContent).toContain('Ordered terminal input is not available from this producer')
    expect([...host.querySelectorAll('button')].map((button) => button.textContent)).not.toContain('Enable input')
    expect(host.textContent).toContain('does not publish terminal history yet')
  })

  it('marks the terminal ended and pauses input when its host is unreachable', async () => {
    await mount({ terminal: observed({ value: screen }), reachability: 'unreachable' })
    expect(host.querySelector('[aria-label="Ended"]')).not.toBeNull()
    expect(host.textContent).toContain('Input is paused while this terminal is hidden, disconnected or stale.')
  })

  it('keeps the last grid but says it is stale', async () => {
    await mount({ terminal: observed({ value: screen, freshness: 'stale' }) })
    expect(host.textContent).toContain('Terminal snapshot is stale.')
    expect(host.querySelector(`[aria-label="Terminal: ${screen.title}"]`)).not.toBeNull()
    expect(host.querySelector('[aria-label="Live"]')).toBeNull()
    expect(host.querySelector('[aria-label="Stale"]')).not.toBeNull()
  })
})
