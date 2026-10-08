// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { Agent, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { DataSourceProvider } from '../data/react.tsx'
import { fixtureSource } from '../data/fixtureSource.ts'
import { observed, unavailable, waiting, type Feed } from '../data/source.ts'
import { terminalSubjectForAgent } from '../data/projections.ts'
import { LiveAgentWorkspace } from './LiveAgentWorkspace.tsx'

vi.mock('@stylexjs/stylex', () => ({ create: (styles: unknown) => styles, defineVars: (variables: unknown) => variables, createTheme: () => ({}), keyframes: () => 'animation', props: () => ({}) }))
const agent = decodeUnknownSync(Agent)({ kind: 'agent', id: 'agent/example', name: 'Example Agent', host_id: 'host/example', runtime_ids: [], reachability: 'reachable', state: 'running', harness_state: 'idle', blocked_on: null, fault: null, revision: '1', updated_at: '2026-10-04T12:00:00.000Z' })
let registry: AtomRegistry.AtomRegistry
let root: Root
let host: HTMLDivElement
beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
  Object.defineProperty(Range.prototype, 'getBoundingClientRect', { configurable: true, value: () => new DOMRect() })
  Object.defineProperty(Range.prototype, 'getClientRects', { configurable: true, value: () => [] })
  window.history.replaceState(null, '', '/w/agent/example?open=thread')
  registry = AtomRegistry.make()
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
})
afterEach(async () => { await act(async () => root.unmount()); registry.dispose(); host.remove(); vi.unstubAllGlobals() })
const mount = async (agents: Feed<readonly Agent[]>) => {
  const source = fixtureSource({ world: { now: 0, agents: [agent], missions: [], attention: [], events: [], conversations: {}, terminals: {}, envelopes: {}, usage: { _tag: 'undeclared' } }, overrides: { agents } })
  await act(async () => root.render(<DataSourceProvider source={source} registry={registry}><LiveAgentWorkspace /></DataSourceProvider>))
}
const toggle = () => host.querySelector<HTMLButtonElement>('[data-testid="thread-header"] button[aria-label="Toggle terminal drawer"]')

describe('agent roster notice', () => {
  it.each([
    ['ungranted', 'Read access to the agent roster has not been granted.'],
    ['unsupported', 'This connection does not support the agent roster.'],
    ['failed', 'The agent roster could not be loaded; reconnect and try again.'],
  ] as const)('states %s with fixed copy and keeps the read detail out of the page', async (reason, copy) => {
    await mount(unavailable({ reason, detail: 'gateway said EACCES at /private/socket' }))
    const notice = host.querySelector(`[data-wf-roster-reason="${reason}"]`)
    expect(notice?.textContent).toBe(copy)
    expect(host.textContent).not.toContain('EACCES')
    expect(host.textContent).not.toContain('/private/socket')
  })
})

describe('terminal navigation toggle', () => {
  it.each([
    ['agent/example-seat', 'terminal/example-seat'],
    ['agent/team/seat', 'terminal/team/seat'],
  ])('keeps %s reachable while the first roster frame is pending', async (id, terminal) => {
    window.history.replaceState(null, '', `/w/${id}?open=thread`)
    await mount(waiting)
    expect(toggle()).not.toBeNull()
    expect(terminalSubjectForAgent(id)).toBe(terminal)
    await act(async () => toggle()!.click())
    expect(new URLSearchParams(window.location.search).get('open')).toBe(`${terminalSubjectForAgent(id)}:detail`)
    expect(toggle()?.getAttribute('aria-pressed')).toBe('true')
    await act(async () => toggle()!.click())
    expect(new URLSearchParams(window.location.search).get('open')).toBe('thread')
    expect(toggle()?.getAttribute('aria-pressed')).toBe('false')
  })
  it.each(['example-seat', 'team/seat'])('does not invent a terminal subject for invalid agent identity %s while waiting', async id => {
    window.history.replaceState(null, '', `/w/${id}?open=thread`)
    await mount(waiting)
    expect(toggle()).toBeNull()
  })
  it('exposes the named toggle on the first header render before the first roster frame', async () => {
    await mount(waiting)
    expect(host.textContent).toContain('Waiting for the agent roster.')
    expect(toggle()).not.toBeNull()
    await act(async () => toggle()!.click())
    expect(new URLSearchParams(window.location.search).get('open')).toBe('terminal/example:detail')
    expect(host.textContent).toContain('Terminal unavailable. This web client has no terminal renderer yet.')
    await act(async () => toggle()!.click())
    expect(new URLSearchParams(window.location.search).get('open')).toBe('thread')
  })
  it('returns to the thread on the second click without faking a terminal renderer', async () => {
    await mount(observed({ value: [agent] }))
    await act(async () => toggle()!.click())
    expect(new URLSearchParams(window.location.search).get('open')).toBe('terminal/example:detail')
    expect(toggle()?.getAttribute('aria-pressed')).toBe('true')
    expect(host.textContent).toContain('Terminal unavailable. This web client has no terminal renderer yet.')
    await act(async () => toggle()!.click())
    expect(new URLSearchParams(window.location.search).get('open')).toBe('thread')
    expect(toggle()?.getAttribute('aria-pressed')).toBe('false')
    expect(host.textContent).not.toContain('Terminal unavailable')
  })
})
