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
import { makeScreen } from '../terminal/fixtures.ts'
import { LiveAgentWorkspace } from './LiveAgentWorkspace.tsx'

vi.mock('@stylexjs/stylex', () => ({ create: (styles: unknown) => styles, defineVars: (variables: unknown) => variables, createTheme: () => ({}), keyframes: () => 'animation', props: () => ({}) }))
const agent = decodeUnknownSync(Agent)({ kind: 'agent', id: 'agent/example', name: 'Example Agent', host_id: 'host/example', runtime_ids: ['runtime/example'], reachability: 'reachable', state: 'running', harness_state: 'idle', blocked_on: null, fault: null, revision: '1', updated_at: '2026-10-04T12:00:00.000Z' })
const screen = { ...makeScreen({ scene: 'F3', columns: 40, rows: 4, frame: 0 }), terminal_id: 'terminal/example' as const }
let registry: AtomRegistry.AtomRegistry
let root: Root
let host: HTMLDivElement
beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
  Object.defineProperty(Range.prototype, 'getBoundingClientRect', { configurable: true, value: () => new DOMRect() })
  Object.defineProperty(Range.prototype, 'getClientRects', { configurable: true, value: () => [] })
  // jsdom has no font loading; the registered terminal faces resolve as loaded.
  const faces = ['Wf Terminal Mono', 'Wf Terminal Nerd Mono'].map((family) => ({ family, load: async () => [] }))
  Object.defineProperty(document, 'fonts', { configurable: true, value: Object.assign(new EventTarget(), { ready: Promise.resolve(), [Symbol.iterator]: () => faces[Symbol.iterator]() }) })
  window.history.replaceState(null, '', '/w/agent/example?open=thread')
  registry = AtomRegistry.make()
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
})
afterEach(async () => { await act(async () => root.unmount()); registry.dispose(); host.remove(); vi.unstubAllGlobals() })
const mount = async (agents: Feed<readonly Agent[]>, terminal: Feed<typeof screen> = observed({ value: screen }), retryTerminal?: (ref: string) => void) => {
  const source = fixtureSource({ world: { now: 0, agents: [agent], missions: [], attention: [], events: [], conversations: { [agent.id]: { items: [], hasOlder: false } }, terminals: {}, envelopes: {}, usage: { _tag: 'undeclared' } }, overrides: { agents, terminal: { 'terminal/example': terminal } } })
  await act(async () => root.render(<DataSourceProvider source={{ ...source, ...(retryTerminal === undefined ? {} : { retryTerminal }) }} registry={registry}><LiveAgentWorkspace /></DataSourceProvider>))
}
const toggle = () => host.querySelector<HTMLButtonElement>('[data-testid="thread-header"] button[aria-label="Toggle terminal drawer"]')
const toggleDescription = () => (toggle()?.getAttribute('aria-describedby') ?? '').split(/\s+/).filter(Boolean)
  .map(id => document.getElementById(id)?.textContent?.trim()).filter(Boolean).join(' | ')
const waitingReason = 'Waiting for the live agent roster to find this agent’s terminal.'
const grid = () => host.querySelector(`[data-testid="terminal-pane"] [aria-label="Terminal: ${screen.title}"]`)

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
  ])('does not offer an unverified terminal for %s before the first roster frame', async (id, terminal) => {
    window.history.replaceState(null, '', `/w/${id}?open=thread`)
    await mount(waiting)
    expect(host.querySelectorAll('button[aria-label="Toggle terminal drawer"]')).toHaveLength(1)
    expect(terminalSubjectForAgent(id)).toBe(terminal)
    expect(toggle()?.disabled).toBe(true)
    expect(toggleDescription()).toBe(waitingReason)
    expect(toggle()?.closest('[title]')?.getAttribute('title')).toBe(waitingReason)
    await act(async () => toggle()!.click())
    expect(new URLSearchParams(window.location.search).get('open')).toBe('thread')
    expect(host.querySelector('[data-testid="terminal-pane"]')).toBeNull()
  })
  it.each(['example-seat', 'team/seat'])('does not invent a terminal subject for invalid agent identity %s while waiting', async id => {
    window.history.replaceState(null, '', `/w/${id}?open=thread`)
    await mount(waiting)
    expect(toggle()).toBeNull()
  })
  it('enables the same named control only after the roster proves a runtime is present', async () => {
    await mount(waiting)
    expect(host.textContent).toContain('Waiting for the agent roster.')
    expect(toggle()?.disabled).toBe(true)
    expect(toggleDescription()).toBe(waitingReason)
    await mount(observed({ value: [agent] }))
    expect(host.querySelectorAll('button[aria-label="Toggle terminal drawer"]')).toHaveLength(1)
    expect(toggle()?.disabled).toBe(false)
    expect(toggleDescription()).toBe('')
    await act(async () => toggle()!.click())
    expect(new URLSearchParams(window.location.search).get('open')).toBe('terminal/example:detail')
    await vi.waitFor(() => expect(grid()).not.toBeNull(), { timeout: 10_000 })
    await act(async () => toggle()!.click())
    expect(new URLSearchParams(window.location.search).get('open')).toBe('thread')
  })
  it('opens the live terminal and returns to the thread on the second click', async () => {
    await mount(observed({ value: [agent] }))
    await act(async () => toggle()!.click())
    expect(new URLSearchParams(window.location.search).get('open')).toBe('terminal/example:detail')
    expect(toggle()?.getAttribute('aria-pressed')).toBe('true')
    await vi.waitFor(() => expect(grid()).not.toBeNull(), { timeout: 10_000 })
    expect(host.textContent).not.toContain('Terminal unavailable')
    await act(async () => toggle()!.click())
    expect(new URLSearchParams(window.location.search).get('open')).toBe('thread')
    expect(toggle()?.getAttribute('aria-pressed')).toBe('false')
    expect(host.querySelector('[data-testid="terminal-pane"]')).toBeNull()
  })
})

describe('terminal failure stays local to the drawer', () => {
  const thread = () => host.querySelector('[data-testid="transcript-scroll"]')
  const composer = () => host.querySelector<HTMLTextAreaElement>('textarea')
  const resources = () => host.querySelector('[data-testid="thread-header"] button[aria-label="Resources"]')
  const breadcrumb = () => host.querySelector('[aria-label="Breadcrumb"]')
  const waitForThread = async () => {
    await vi.waitFor(async () => {
      await act(async () => {})
      expect(thread()).not.toBeNull()
      expect(composer()).not.toBeNull()
    }, { timeout: 15_000 })
  }
  const retained = () => ({ thread: thread(), composer: composer(), resources: resources(), breadcrumb: breadcrumb() })
  interface RetainedThread {
    readonly thread: Element | null
    readonly composer: HTMLTextAreaElement | null
    readonly resources: Element | null
    readonly breadcrumb: Element | null
  }
  const expectThreadRetained = (before: RetainedThread) => {
    expect(retained()).toEqual(before)
    expect(resources()).not.toBeNull()
    expect(breadcrumb()?.textContent).toBe('example/Example Agent')
    expect(composer()?.closest('form')?.inert).not.toBe(true)
    expect(thread()?.closest('[aria-hidden]')?.getAttribute('aria-hidden')).toBe('false')
    expect(host.textContent).not.toMatch(/\b(undefined|unknown)\b/i)
  }
  it('never enables the first visible control for an agent the roster does not list', async () => {
    window.history.replaceState(null, '', '/w/agent/example/no-terminal?open=thread')
    await mount(waiting)
    expect(host.querySelectorAll('button[aria-label="Toggle terminal drawer"]')).toHaveLength(1)
    expect(toggle()?.disabled).toBe(true)
    expect(toggleDescription()).toBe(waitingReason)
    await act(async () => toggle()!.click())
    expect(new URLSearchParams(window.location.search).get('open')).toBe('thread')
    await mount(observed({ value: [agent] }))
    const reason = 'This agent is not in the live agent roster, so it has no terminal.'
    expect(toggle()?.disabled).toBe(true)
    expect(toggleDescription()).toBe(reason)
    expect(toggle()?.closest('[title]')?.getAttribute('title')).toBe(reason)
    await act(async () => toggle()!.click())
    expect(new URLSearchParams(window.location.search).get('open')).toBe('thread')
    expect(host.querySelector('[data-testid="terminal-pane"]')).toBeNull()
    expect(host.textContent).not.toMatch(/\b(undefined|unknown)\b/i)
  })
  it('disables a known terminal-less agent with a reason tooltip and leaves its thread usable', async () => {
    await mount(observed({ value: [{ ...agent, runtime_ids: [] }] }))
    await waitForThread()
    const before = retained()
    expect(toggle()?.disabled).toBe(true)
    expect(host.querySelector('[data-testid="terminal-toggle-disabled"]')?.getAttribute('title')).toBe('This agent has no terminal.')
    await act(async () => toggle()!.click())
    expect(host.querySelector('[data-testid="terminal-pane"]')).toBeNull()
    expectThreadRetained(before)
  })
  it('keeps a terminal-less deep link inside a drawer beside the thread', async () => {
    window.history.replaceState(null, '', '/w/agent/example?open=terminal/example:detail')
    const feed = { ...unavailable({ reason: 'failed', detail: 'This agent has no terminal.', code: 'no-terminal' }), retryable: false }
    await mount(observed({ value: [{ ...agent, runtime_ids: [] }] }), feed)
    await waitForThread()
    await vi.waitFor(() => expect(host.querySelector('[data-testid="terminal-pane"]')?.textContent).toContain('This agent has no terminal.'))
    expect(host.querySelector('[data-testid="terminal-pane"]')?.getAttribute('aria-label')).toBe('Terminal drawer')
    expectThreadRetained(retained())
    expect(host.querySelector('[data-testid="terminal-pane"]')?.textContent).not.toContain('Retry')
  })
  it.each([
    ['ungranted', 'Terminal viewing is refused by the gateway.', false],
    ['failed', 'The terminal service is temporarily unavailable.', true],
  ] as const)('shows %s reason and only offers a useful Retry without replacing the thread', async (reason, detail, retryable) => {
    const retry = vi.fn()
    const feed = { ...unavailable({ reason, detail }), retryable }
    await mount(observed({ value: [agent] }), feed, retry)
    await waitForThread()
    const before = retained()
    await act(async () => toggle()!.click())
    await vi.waitFor(() => expect(host.querySelector('[data-testid="terminal-pane"]')?.textContent).toContain(detail))
    expectThreadRetained(before)
    const retryButton = [...host.querySelectorAll<HTMLButtonElement>('[data-testid="terminal-pane"] button')].find(button => button.textContent === 'Retry')
    expect(retryButton !== undefined).toBe(retryable)
    if (retryable) {
      await act(async () => retryButton!.click())
      expect(retry).toHaveBeenCalledWith('terminal/example')
      expectThreadRetained(before)
    }
    await act(async () => toggle()!.click())
    expect(host.querySelector('[data-testid="terminal-pane"]')).toBeNull()
    expectThreadRetained(before)
  })
})
