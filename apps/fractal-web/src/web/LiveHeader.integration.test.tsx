// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { Agent, AgentQueue, ResourceObservation, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { DataSourceProvider } from '../data/react.tsx'
import { fixtureSource } from '../data/fixtureSource.ts'
import { observed, unavailable, type Feed } from '../data/source.ts'
import type { ResourcePage } from '../resources/agent/model.ts'
import { LiveAgentWorkspace } from './LiveAgentWorkspace.tsx'
import type { ConversationPage } from '../data/source.ts'
import { decodeConversationChunk } from '@st3/sdk/effect'
import { LiveTimeline } from '../conversation/fromTimeline.ts'
import { createConversationTranscript } from './conversationTranscript.ts'

vi.mock('@stylexjs/stylex', () => ({ create: (styles: unknown) => styles, defineVars: (variables: unknown) => variables, createTheme: () => ({}), keyframes: () => 'animation', props: () => ({}) }))

const agent = decodeUnknownSync(Agent)({ kind: 'agent', id: 'agent/example', name: 'Example Agent', host_id: 'host/example', runtime_ids: [], reachability: 'reachable', state: 'running', harness_state: 'idle', blocked_on: null, fault: null, revision: '1', updated_at: '2026-10-04T12:00:00.000Z' })
const queue = decodeUnknownSync(AgentQueue)({ agent_id: agent.id, current_work_ids: ['work/current'], kind: 'agent-queue', move_count: 0, moves: [], next_work_id: null, runs: [{ claimed_work_ids: [], joined_at: '2026-10-04T12:00:00.000Z', mission_run_id: 'mission-run/example', position: 1, ready_work_ids: ['work/ready'], run_state: 'running', state: 'ready', waiting_work_ids: [] }] })
const resource = decodeUnknownSync(ResourceObservation)({ id: 'resource/example', kind: 'filesystem.file', facts: { title: 'Observed project notes', state: 'open' }, observed_at: '2026-10-04T12:00:00.000Z', opened_by: agent.id, opened_by_run: null })
const resources = Atom.make<Feed<ResourcePage>>(observed({ value: { items: [resource], nextCursor: null } }))
const conversation = Atom.make<Feed<ConversationPage>>(observed<ConversationPage>({ value: { items: [
  { _tag: 'UnknownEvent', id: 'internal', eventType: 'credential_pin', data: { pin_hint: 'not-for-display' } },
  { _tag: 'UnknownEvent', id: 'future', eventType: 'future_kind', data: {} },
], hasOlder: false, observation: { empty: false } } }))
const source = {
  ...fixtureSource({ world: { now: Date.parse('2026-10-06T12:00:00.000Z'), agents: [agent], missions: [], attention: [], events: [], conversations: {}, terminals: {}, envelopes: {}, usage: { _tag: 'undeclared' }, subjectReads: { agentQueue: { [agent.id]: observed({ value: queue }) } } } }),
  conversation: (_ref: string) => conversation,
  gateway: 'http://fixture.invalid',
  resources: { byAgent: () => resources, byId: () => Atom.make(observed({ value: resource })), subjects: Atom.make([]), loadMore: vi.fn(), refresh: vi.fn(), resolveFile: () => undefined },
}
let registry: AtomRegistry.AtomRegistry
let root: Root
let host: HTMLDivElement
const button = (name: string) => [...document.querySelectorAll('button')].find(node => (node.getAttribute('aria-label') ?? node.textContent?.trim()) === name)
const click = async (name: string) => { const node = button(name); expect(node, name).toBeDefined(); await act(async () => node!.click()) }

beforeEach(async () => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
  // React Aria uses CSS.escape for generated collection keys; jsdom does not supply it.
  vi.stubGlobal('CSS', { escape: (value: string) => Array.from(value, char => /[a-zA-Z_-]/.test(char) ? char : `\\${char.codePointAt(0)?.toString(16)} `).join('') })
  Object.defineProperty(Range.prototype, 'getBoundingClientRect', { configurable: true, value: () => new DOMRect() })
  Object.defineProperty(Range.prototype, 'getClientRects', { configurable: true, value: () => [] })
  window.history.replaceState(null, '', '/w/agent/example?open=thread')
  registry = AtomRegistry.make()
  registry.set(resources, observed({ value: { items: [resource], nextCursor: null } }))
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
  await act(async () => root.render(<DataSourceProvider source={source} registry={registry}><LiveAgentWorkspace /></DataSourceProvider>))
})
afterEach(async () => { await act(async () => root.unmount()); registry.dispose(); host.remove(); vi.unstubAllGlobals() })

describe('live header parity', () => {
  it('renders named Resources and Conversation controls in the owning header', async () => {
    const header = host.querySelector('[data-testid="thread-header"]')!
    const names = [...header.querySelectorAll('button')].map(node => node.getAttribute('aria-label') ?? node.textContent?.trim())
    expect(names).toEqual(['Resources', 'Conversation menu', 'Toggle terminal drawer', 'Toggle right panel'])
    await click('Conversation menu')
    const setting = document.querySelector('[role="menuitemcheckbox"]')
    expect(setting?.textContent).toContain('Show all system events')
    expect(setting?.getAttribute('aria-checked')).toBe('false')
    // The conversation pane loads lazily; wait for its first transcript frame.
    await vi.waitFor(() => expect(host.querySelector('[data-testid="transcript-empty"]')).not.toBeNull(), { timeout: 15_000 })
    await act(async () => setting?.dispatchEvent(new MouseEvent('click', { bubbles: true })))
    await click('Conversation menu')
    expect(document.querySelector('[role="menuitemcheckbox"]')?.getAttribute('aria-checked')).toBe('true')
    expect(host.textContent?.match(/An event this view cannot show yet\./g)).toHaveLength(2)
    expect(host.textContent).not.toMatch(/credential_pin|future_kind|not-for-display/)
    await act(async () => document.querySelector('[role="menuitemcheckbox"]')?.dispatchEvent(new MouseEvent('click', { bubbles: true })))
    expect(host.textContent).not.toContain('An event this view cannot show yet.')
  })
  it('opens real resources and queue observations, and closes their inspector', async () => {
    await click('Resources')
    const dialog = document.querySelector('[role="dialog"][aria-label="Agent resources"]')
    expect(dialog?.textContent).toContain('Observed project notes')
    const row = dialog?.querySelector('[aria-label="Observed resources"] li')
    expect(row?.textContent).toContain('filesystem.file')
    expect(row?.textContent).toContain('open')
    expect(row?.textContent).toContain('2d ago')
    expect(row?.querySelector('time')?.getAttribute('datetime')).toBe('2026-10-04T12:00:00.000Z')
    expect(dialog?.textContent).toContain('1 current work item')
    expect(dialog?.textContent).toContain('mission-run/example')
    await click('Close resources')
    expect(document.querySelector('[role="dialog"][aria-label="Agent resources"]')).toBeNull()
  })
  it('opens successful change captures from the public conversation feed', async () => {
    const timeline = new LiveTimeline()
    timeline.apply(decodeConversationChunk({
      kind: 'conversation', collection: 'conversation', id: 'follow/example',
      session_id: 'session/example', replace: true, has_more: true, items: [
        { id: 'timeline-entry/edit', sequence: 1, revision: 1, final: true, role: 'assistant',
          timestamp: '2026-10-04T12:00:00.000Z', type: 'tool_call',
          body: { call_id: 'call/edit', name: 'Edit', arguments: { file_path: 'src/example.ts', old_string: 'beforeCapture', new_string: 'afterCapture' } } },
        { id: 'timeline-entry/result', sequence: 2, revision: 1, final: true, role: 'tool',
          timestamp: '2026-10-04T12:00:01.000Z', type: 'tool_result',
          body: { call_id: 'call/edit', status: 'success', media_type: 'text/plain', content: 'Edit applied' } },
      ],
    }))
    expect(timeline.project().items[0]).toMatchObject({ _tag: 'ToolCall', status: 'success' })
    // Hold the fixture atom while the lazy thread/inspector chunks acquire their own subscriptions.
    const release = registry.mount(source.conversation(agent.id))
    try {
      await act(async () => registry.set(conversation, observed({ value: {
        items: timeline.project().items, hasOlder: timeline.hasOlder,
      } })))
      await click('Toggle right panel')
      await vi.waitFor(async () => {
        await act(async () => {})
        const inspector = host.querySelector('aside[aria-label="Changes"]')
        expect(inspector?.textContent).toContain('Successful-tool captures · loaded transcript window')
        expect(inspector?.textContent).toContain('older history not shown')
        expect(inspector?.textContent).toContain('src/example.ts')
        expect(inspector?.textContent).toContain('beforeCapture')
        expect(inspector?.textContent).toContain('afterCapture')
        expect(inspector?.querySelector('[data-captured-change]')).not.toBeNull()
        expect(inspector?.querySelector('button[aria-label="Open src/example.ts"]')).not.toBeNull()
        expect(inspector?.textContent).not.toContain('This web client does not read change data yet')
      }, { timeout: 15_000 })
    } finally {
      release()
    }
  })
  it('persists the system-event view preference without any connection or agent identity in storage keys', async () => {
    await click('Conversation menu')
    await act(async () => document.querySelector('[role="menuitemcheckbox"]')?.dispatchEvent(new MouseEvent('click', { bubbles: true })))
    // The persistence boundary flushes on pagehide; no debounce sleeps are needed.
    window.dispatchEvent(new Event('pagehide'))
    const stored = window.localStorage.getItem('wf.ui@1') ?? ''
    expect(stored).toContain('"conversation.systemEvents":')
    expect(stored).not.toContain(agent.id)
    expect(stored).not.toContain(source.label)
    expect(stored).not.toContain(source.gateway)
    expect(Object.keys(window.localStorage).join(' ')).not.toContain(agent.id)
    expect(Object.keys(window.localStorage).join(' ')).not.toContain(source.label)
    expect(Object.keys(window.localStorage).join(' ')).not.toContain(source.gateway)
    await click('Conversation menu')
    await act(async () => document.querySelector('[role="menuitemcheckbox"]')?.dispatchEvent(new MouseEvent('click', { bubbles: true })))
  })
  it('retains a disabled resources control with a human refusal reason', async () => {
    await act(async () => registry.set(resources, unavailable({ reason: 'ungranted', detail: 'raw-denial-code' })))
    expect(button('Resources')?.disabled).toBe(true)
    const reason = document.getElementById(button('Resources')!.getAttribute('aria-describedby')!)
    expect(reason?.textContent).toBe('Read access to agent resources has not been granted.')
    expect(host.querySelector('[data-testid="thread-header"]')?.textContent).not.toMatch(/raw-denial-code|Unknown/)
  })
})

it('invalidates cached transcript rows when the system-event preference changes without exposing kinds or payloads', () => {
  const project = createConversationTranscript()
  const feed = observed({ value: { items: [{ _tag: 'UnknownEvent' as const, id: 'internal', eventType: 'credential_pin', data: { private: 'not-for-display' } }, { _tag: 'UnknownEvent' as const, id: 'future', eventType: 'future_kind', data: {} }], hasOlder: false, observation: { empty: false } } })
  const hidden = project(feed, { showSystemEvents: false })
  const shown = project(feed, { showSystemEvents: true })
  expect(hidden._tag).toBe('Observed')
  expect(shown._tag).toBe('Observed')
  if (hidden._tag !== 'Observed' || shown._tag !== 'Observed') return
  expect(hidden.items).toEqual([])
  expect(shown.items.map(item => item.id)).toEqual(['internal', 'future'])
  expect(shown.items.every(item => item._tag === 'Notice' && item.text === 'An event this view cannot show yet.')).toBe(true)
  expect(JSON.stringify(shown)).not.toMatch(/credential_pin|future_kind|not-for-display/)
  expect(project(feed, { showSystemEvents: false })._tag === 'Observed').toBe(true)
  expect(project(feed, { showSystemEvents: true })).toBe(project(feed, { showSystemEvents: true }))
})
