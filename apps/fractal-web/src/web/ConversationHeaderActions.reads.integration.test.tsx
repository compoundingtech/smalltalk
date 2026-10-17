// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import * as Atom from 'effect/reactivity/Atom'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { DataSourceProvider } from '../data/react.tsx'
import { fixtureSource } from '../data/fixtureSource.ts'
import { gatewayResources } from '../resources/agent/source.ts'
import { ConversationHeaderActions, retainedResourceAtoms } from './ConversationHeaderActions.tsx'
import { AgentTodos } from '../conversation/todos/AgentTodos.tsx'
import { todoAgent, todoAgentRef } from '../conversation/todos/fixtures.ts'
import { projectTodos } from '../conversation/todos/model.ts'
import { observed } from '../data/source.ts'

vi.mock('@stylexjs/stylex', () => import('../testStylex.ts'))
const snapshot = { id: 'snapshot/example', host_id: 'host/example', store_index: 1, projection_version: 'client-projection.v0', created_at: '2026-10-09T00:00:00Z' }
let root: Root
let host: HTMLDivElement
let registry: AtomRegistry.AtomRegistry
const requests: string[] = []
const fetchImpl: typeof fetch = async input => {
  const path = new URL(String(input)).pathname
  requests.push(path)
  const value = path === '/v1/client/capabilities'
    ? { kind: 'capabilities', capabilities: [], limits: { max_page_items: 100, max_event_items: 100, max_wait_ms: 1000, max_response_bytes: 65536 }, schemas: [] }
    : { kind: 'resources-page', items: [], page: { limit: 50, has_more: false, next_cursor: null } }
  return new Response(JSON.stringify({ api_version: 'st3.client.v0', snapshot, value }), { headers: { 'content-type': 'application/json' } })
}
const source = {
  ...fixtureSource({ world: { now: 0, agents: [], missions: [], attention: [], events: [], conversations: {}, terminals: {}, envelopes: {}, usage: { _tag: 'undeclared' } } }),
  resources: gatewayResources({ baseUrl: 'https://alpha.example', fetchImpl }),
}
const show = async (ref: string) => {
  await act(async () => root.render(<DataSourceProvider source={source} registry={registry}><ConversationHeaderActions key={ref} agentRef={ref} /></DataSourceProvider>))
}
const settle = async () => { await act(async () => { for (let i = 0; i < 10; i++) await new Promise<void>(resolve => setImmediate(resolve)) }) }
const click = async (name: string) => {
  const button = [...document.querySelectorAll('button')].find(node => (node.getAttribute('aria-label') ?? node.textContent) === name)
  expect(button).toBeDefined()
  await act(async () => button!.click())
  await settle()
}
beforeEach(() => {
  requests.length = 0
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
  registry = AtomRegistry.make()
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
})
afterEach(async () => { await act(async () => root.unmount()); registry.dispose(); host.remove(); vi.unstubAllGlobals(); vi.useRealTimers() })
it('adds no serial HTTP requests to a seat switch while Resources is closed', async () => {
  await show('agent/example')
  await settle()
  await show('agent/other')
  await settle()
  expect(requests).toEqual([])
  await click('Resources')
  expect(requests).toEqual(['/v1/client/capabilities', '/v1/client/resources'])
})
it('keeps the authoritative poll alive for 60 seconds after closing, then releases it', async () => {
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
  await show('agent/example')
  await click('Resources')
  await click('Close resources')
  requests.length = 0
  await act(async () => vi.advanceTimersByTimeAsync(30_000))
  await settle()
  expect(requests.filter(path => path === '/v1/client/resources').length).toBeGreaterThan(0)
  await act(async () => vi.advanceTimersByTimeAsync(31_000))
  await settle()
  requests.length = 0
  await act(async () => vi.advanceTimersByTimeAsync(30_000))
  await settle()
  expect(requests).toEqual([])
})
it('reopens within the TTL with the retained Observed value and no second read', async () => {
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
  let reads = 0
  const resource = Atom.make(() => {
    reads++
    return observed({ value: { items: [], nextCursor: null } })
  })
  const retainedSource = { ...source, resources: { ...source.resources, byAgent: () => resource } }
  const atom = retainedResourceAtoms(retainedSource).resources('agent/negative-control')
  let release = registry.mount(atom)
  const first = registry.get(atom)
  expect(first._tag).toBe('Observed')
  release()
  await vi.advanceTimersByTimeAsync(1000)
  release = registry.mount(atom)
  expect(registry.get(atom)).toBe(first)
  expect(reads).toBe(1)
  release()
})
it('prefetches on explicit focus while keeping the sheet closed', async () => {
  await show('agent/example')
  await act(async () => host.querySelector<HTMLButtonElement>('button[aria-label="Resources"]')!.focus())
  await settle()
  expect(document.querySelector('aside[aria-label="Agent resources"]')).toBeNull()
  expect(requests).toEqual(['/v1/client/capabilities', '/v1/client/resources'])
})
it('shows the agent Todos as the first Resources section, naming a missing snapshot', async () => {
  await show('agent/example')
  await click('Resources')
  const sections = [...document.querySelectorAll('aside[aria-label="Agent resources"] section[aria-label]')]
  expect(sections.map(section => section.getAttribute('aria-label'))).toEqual(['Todos', 'Agent queue', 'Observed resources'])
  expect(sections[0]!.textContent).toBe('TodosNo harness todo snapshot observed. This is not an empty todo list.')
})
it('keeps the Todos disclosure with phase, progress and harness inside Resources', async () => {
  const state = projectTodos({ feed: observed({ value: [todoAgent] }), agentRef: todoAgentRef })
  expect(state._tag).toBe('Observed')
  await act(async () => root.render(<AgentTodos state={state} placement="resources" />))
  const disclosure = host.querySelector('details[aria-label="Harness todos"]') as HTMLDetailsElement
  expect(disclosure.open).toBe(false)
  const summary = disclosure.querySelector('summary')!.textContent!
  if (state._tag !== 'Observed') return
  expect(summary).toContain(`${state.completed}/${state.total}`)
  expect(summary).toContain(state.todo.snapshot.harness)
})
it('keeps the Resources panel inside its owning scheme root', async () => {
  host.dataset.scheme = 'light'
  await show(todoAgentRef)
  await click('Resources')
  const panel = document.querySelector('aside[aria-label="Agent resources"]')!
  expect(panel.closest('[data-scheme]')).toBe(host)
  expect(panel.closest('[aria-hidden="true"]')).toBeNull()
})
it('opens Resources as a non-modal complementary panel that returns focus on Escape', async () => {
  await show(todoAgentRef)
  const trigger = host.querySelector<HTMLButtonElement>('button[aria-label="Resources"]')!
  await act(async () => trigger.focus())
  await click('Resources')
  const panel = document.querySelector<HTMLElement>('aside[aria-label="Agent resources"]')!
  // A labelled complementary landmark, never a dialog: nothing outside it is inerted or hidden.
  expect(panel.getAttribute('role')).toBeNull()
  expect(panel.getAttribute('aria-modal')).toBeNull()
  expect(document.querySelector('[role="dialog"]')).toBeNull()
  expect(document.querySelectorAll('[inert]').length).toBe(0)
  expect(trigger.closest('[aria-hidden="true"]')).toBeNull()
  expect(document.querySelectorAll('body > [aria-hidden="true"]').length).toBe(0)
  expect(trigger.getAttribute('aria-expanded')).toBe('true')
  expect(trigger.getAttribute('aria-controls')).toBe(panel.id)
  expect(panel.contains(document.activeElement)).toBe(true)
  await act(async () => { document.activeElement!.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true })) })
  // React Aria restores focus on the frame after the scope unmounts.
  await act(() => new Promise<void>(resolve => requestAnimationFrame(() => resolve())))
  expect(document.querySelector('aside[aria-label="Agent resources"]')).toBeNull()
  expect(document.activeElement).toBe(trigger)
  expect(trigger.getAttribute('aria-expanded')).toBe('false')
})

it('shows pending binding verification instead of Stale while the roster request is pending', async () => {
  const state = projectTodos({
    feed: observed({ value: [todoAgent], freshness: 'stale' }),
    agentRef: todoAgentRef,
    sync: { _tag: 'Requested', since: 0 },
  })
  await act(async () => root.render(<AgentTodos state={state} placement="resources" />))
  const summary = host.querySelector('summary')!
  expect(summary.textContent).toContain('Pending · omp')
  expect(summary.textContent).not.toContain('Stale')
  expect(host.textContent).toContain('binding verification pending')
})
