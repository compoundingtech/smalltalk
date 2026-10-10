// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { DataSourceProvider } from '../data/react.tsx'
import { fixtureSource } from '../data/fixtureSource.ts'
import { gatewayResources } from '../resources/agent/source.ts'
import { ConversationHeaderActions } from './ConversationHeaderActions.tsx'

vi.mock('@stylexjs/stylex', () => ({ create: (styles: unknown) => styles, defineVars: (variables: unknown) => variables, createTheme: () => ({}), keyframes: () => 'test-animation', props: () => ({}) }))
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
it('ends the resource poll when its inspector closes, not on the next seat switch', async () => {
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
  await show('agent/example')
  await click('Resources')
  await click('Close resources')
  requests.length = 0
  await act(async () => vi.advanceTimersByTimeAsync(30_000))
  await settle()
  expect(requests).toEqual([])
})
