// @vitest-environment jsdom
/**
 * The pane owns conversation demand: a cold mount on a route-named agent follows through the
 * real SDK and live source (scripted gateway, real socket commands), and switching agents
 * moves that demand with the keyed pane. Nothing here mocks the data hooks.
 */
import type { Agent, CollectionCommand, CollectionFrame, CollectionSocket, Snapshot, TimelineEntry } from '@smalltalk/st3-client'
import { flushSync } from 'react-dom'
import { createRoot, type Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
// The kit compiles StyleX at build time; node tests stub only the CSS runtime, never data hooks.
vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}),
  keyframes: () => 'test-animation',
  props: () => ({}),
}))
import { liveSource, type LiveSource } from '../data/liveSource.ts'
import { DataSourceProvider } from '../data/react.tsx'
import { ConversationPane } from './ConversationPane.tsx'
import { LiveAgentWorkspace } from './LiveAgentWorkspace.tsx'

const snapshot: Snapshot = {
  id: 'snapshot/1',
  created_at: '2026-10-03T00:00:00Z',
  host_id: 'host/example',
  projection_version: 'client-projection.v0',
  store_index: 1,
}

const agent: Agent = {
  id: 'agent/route',
  kind: 'agent',
  revision: '1',
  updated_at: snapshot.created_at,
  name: 'Route',
  state: 'running',
  reachability: 'local',
  runtime_ids: [],
}

const entry = (sequence: number, text: string, role: 'user' | 'assistant' = 'assistant'): TimelineEntry => ({
  id: `timeline-entry/${sequence}`,
  sequence,
  revision: 1,
  type: 'content',
  role,
  final: true,
  timestamp: snapshot.created_at,
  body: { text, media_type: 'text/markdown' },
})

/** The kit composition groups turns at user prompts, so scenarios pair prompts with replies. */
const prompt = (sequence: number, text: string) => entry(sequence, text, 'user')

/** A native scenario page as st delivers it: joined tool result, custom omp event, truncation. */
const nativeScenario = (): TimelineEntry[] => [
  prompt(1, 'Keep the row projection readable.'),
  {
    id: 'timeline-entry/2', sequence: 2, revision: 1, type: 'tool_call', role: 'assistant', final: true,
    timestamp: snapshot.created_at, body: { call_id: 'c1', name: 'read', arguments: { path: 'src/rows.ts' } },
  },
  {
    id: 'timeline-entry/3', sequence: 3, revision: 1, type: 'tool_result', role: 'tool', final: true,
    timestamp: snapshot.created_at,
    body: { call_id: 'c1', content: 'export const rows = []', media_type: 'text/typescript', status: 'success' },
  },
  entry(4, 'The projection keeps **visible rows** together.'),
  {
    id: 'timeline-entry/5', sequence: 5, revision: 1, type: 'content', role: 'system', final: true,
    timestamp: snapshot.created_at,
    body: { text: '[unrecognized omp entry `credential_pin`]\n{"kind":"credential_pin"}', media_type: 'text/plain' },
  },
  {
    id: 'timeline-entry/6', sequence: 6, revision: 1, type: 'truncation', role: 'system', final: true,
    timestamp: snapshot.created_at,
    body: { reason: 'retained transcript', omitted_from_sequence: 0, omitted_to_sequence: 0 },
  },
]

class Gateway {
  socket: CollectionSocket | undefined
  readonly commands: CollectionCommand[] = []

  readonly fetch: typeof fetch = async (input) => {
    const path = new URL(String(input)).pathname
    if (path !== '/v1/client/capabilities') throw new Error(`Unexpected request ${path}`)
    return new Response(
      JSON.stringify({
        api_version: 'st3.client.v0',
        snapshot,
        value: {
          kind: 'capabilities',
          capabilities: [{ id: 'work.done', state: 'granted', version: 0 }],
          event_cursor: 'cursor/current',
          limits: { max_page_items: 100, max_event_items: 100, max_wait_ms: 1000, max_response_bytes: 65536 },
          schemas: ['client-v0'],
          session_actor: 'person/operator',
          transport: 'fabric-loopback',
        },
      }),
      { status: 200, headers: { 'content-type': 'application/json' } },
    )
  }

  readonly factory = () => {
    const socket: CollectionSocket = {
      onopen: null,
      onmessage: null,
      onclose: null,
      onerror: null,
      send: (text: string) => this.commands.push(JSON.parse(text)),
      close: () => {},
    }
    this.socket = socket
    queueMicrotask(() => socket.onopen?.())
    return socket
  }

  conversationSubscription(ref: string) {
    const command = this.commands.findLast(
      (candidate): candidate is Extract<CollectionCommand, { kind: 'subscribe'; collection: 'conversation' }> =>
        candidate.kind === 'subscribe' && candidate.collection === 'conversation' && candidate.conversation === ref,
    )
    if (command === undefined) throw new Error(`No conversation subscription for ${ref}`)
    return command
  }

  subscribesFor(ref: string) {
    return this.commands.filter(
      (candidate) => candidate.kind === 'subscribe' && candidate.collection === 'conversation' && candidate.conversation === ref,
    ).length
  }

  unsubscribedRefs() {
    const conversationById = new Map<string, string>()
    for (const command of this.commands)
      if (command.kind === 'subscribe' && command.collection === 'conversation' && command.conversation !== undefined)
        conversationById.set(command.id, command.conversation)
    return this.commands
      .filter((command) => command.kind === 'unsubscribe')
      .map((command) => conversationById.get(command.id))
      .filter((ref): ref is string => ref !== undefined)
  }

  send(frame: CollectionFrame) {
    this.socket?.onmessage?.({ data: JSON.stringify(frame) })
  }

  conversationFrame(ref: string, rows: TimelineEntry[], hasMore = false) {
    this.send({
      kind: 'conversation',
      id: this.conversationSubscription(ref).id,
      collection: 'conversation',
      session_id: `session/${ref}`,
      items: rows,
      replace: true,
      has_more: hasMore,
    })
  }
}

let frames = new Map<number, FrameRequestCallback>()
let live: LiveSource | undefined
let gateway: Gateway | undefined
let root: Root | undefined
const container = document.createElement('div')

beforeEach(() => {
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
  frames = new Map()
  let nextFrame = 0
  vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
    const id = (nextFrame += 1)
    frames.set(id, callback)
    return id
  })
  vi.stubGlobal('cancelAnimationFrame', (id: number) => frames.delete(id))
  // jsdom has no layout observers; the kit instantiates one while attaching scroll.
  // Test-environment sizing (react-aria renders every row) needs no measured entries.
  vi.stubGlobal('ResizeObserver', class {
    observe() {}
    unobserve() {}
    disconnect() {}
  })
  document.body.append(container)
})

afterEach(async () => {
  if (root !== undefined) flushSync(() => root!.unmount())
  root = undefined
  await live?.dispose()
  live = undefined
  gateway = undefined
  container.remove()
  vi.unstubAllGlobals()
  vi.useRealTimers()
})

/** Drain socket callbacks, browser-task decode slices, stream fibers and frame-ingest commits. */
const settle = async () => {
  for (let round = 0; round < 10; round += 1) {
    await new Promise<void>((resolve) => setImmediate(resolve))
    await vi.advanceTimersByTimeAsync(1)
    const pending = [...frames.values()]
    frames.clear()
    for (const callback of pending) callback(0)
  }
}

const until = async (check: () => boolean, maxRounds = 400) => {
  for (let round = 0; round < maxRounds && !check(); round += 1) await settle()
  if (!check()) throw new Error('condition not reached before the drain budget ran out')
}

const open = (conversationSlots: number | 'advertised' = 'advertised') => {
  gateway = new Gateway()
  live = liveSource({
    options: { baseUrl: 'http://gateway.test', maxFollows: 8, conversationSlots, socket: gateway.factory, fetch: gateway.fetch },
  })
  return { live, gateway }
}

const mountPane = (agentRef: string) => {
  root ??= createRoot(container)
  flushSync(() => {
    root!.render(
      <DataSourceProvider source={live!.source} registry={live!.registry}>
        <ConversationPane key={agentRef} agentRef={agentRef} agentName="Route" onOpenTool={() => {}} />
      </DataSourceProvider>,
    )
  })
}

const paneText = () => container.textContent ?? ''

describe('ConversationPane conversation demand', () => {
  it('follows a cold route-named agent on mount, reaches Live and renders its message', async () => {
    open()
    expect(gateway!.subscribesFor('agent/route')).toBe(0)
    mountPane('agent/route')
    await until(() => gateway!.subscribesFor('agent/route') === 1)
    // Before the first conversation observation the kit skeleton waits inside the lane.
    expect(container.querySelector('[data-testid="transcript-placeholder"]')).not.toBeNull()
    expect(paneText()).not.toContain('Waiting for the first conversation observation')
    gateway!.conversationFrame('agent/route', [prompt(1, 'Cold route ask'), entry(2, 'Cold route hello')])
    await until(() => live!.registry.get(live!.source.sync!.conversation('agent/route')).sync.status._tag === 'Live')
    await until(() => paneText().includes('Cold route hello'))
    expect(gateway!.unsubscribedRefs()).toEqual([])
  })

  it('renders a native scenario page through the SDK fold into the kit composition', async () => {
    open()
    mountPane('agent/route')
    await until(() => gateway!.subscribesFor('agent/route') === 1)
    gateway!.conversationFrame('agent/route', nativeScenario(), true)
    await until(() => paneText().includes('visible rows'))
    expect(container.querySelectorAll('[data-testid="transcript-turn"]')).toHaveLength(1)
    expect(container.querySelector('[data-testid="user-message"]')?.textContent).toContain('Keep the row projection readable')
    expect(container.querySelector('[data-testid="agent-message"]')?.querySelector('strong')?.textContent).toBe('visible rows')
    expect(container.querySelector('[data-testid="work-log"]')).not.toBeNull()
    expect(container.querySelector('[data-testid="history-boundary"]')?.textContent).toContain('Earlier messages not loaded')
    // The unsupported omp event and the truncation marker never become rows or internal wording.
    expect(paneText()).not.toContain('credential_pin')
    expect(paneText()).not.toContain('sequences')
    expect(paneText()).not.toContain('Older history unavailable')
  })

  it('renders the live workspace transcript and opens and closes its sibling tool panel', async () => {
    window.history.replaceState(null, '', '/w/agent/route')
    open()
    root = createRoot(container)
    flushSync(() => {
      root!.render(
        <DataSourceProvider source={live!.source} registry={live!.registry}>
          <LiveAgentWorkspace />
        </DataSourceProvider>,
      )
    })
    await until(() => gateway!.subscribesFor('agent/route') === 1)
    gateway!.conversationFrame('agent/route', nativeScenario(), true)
    await until(() => paneText().includes('visible rows'))
    const workspace = container.querySelector('section[aria-label="Agent workspace"]')!
    expect(workspace.querySelector('[data-testid="transcript-turn"]')).not.toBeNull()
    expect(container.querySelector('aside[aria-label="Tool detail"]')).toBeNull()
    const button = container.querySelector('button[aria-label="Open read tool detail"]') as HTMLButtonElement
    expect(button).not.toBeNull()
    flushSync(() => button.click())
    const panel = container.querySelector('aside[aria-label="Tool detail"]')!
    expect(panel.textContent).toContain('export const rows = []')
    expect(panel.parentElement).toBe(workspace.parentElement)
    expect(workspace.contains(panel)).toBe(false)
    flushSync(() => (panel.querySelector('button[aria-label="Close tool detail"]') as HTMLButtonElement).click())
    expect(container.querySelector('aside[aria-label="Tool detail"]')).toBeNull()
    expect(workspace.querySelector('[data-testid="agent-message"]')).not.toBeNull()
    window.history.replaceState(null, '', '/')
  })

  it('moves demand with the pane across back/forward agent switches', async () => {
    // One conversation slot: the warm pool cannot retain the released thread, so switching
    // demand must evict the previous agent's follow with a real unsubscribe.
    open(1)
    mountPane('agent/route')
    await until(() => gateway!.subscribesFor('agent/route') === 1)
    gateway!.conversationFrame('agent/route', [prompt(1, 'First ask'), entry(2, 'First thread')])
    await until(() => paneText().includes('First thread'))

    mountPane('agent/other')
    await until(() => gateway!.subscribesFor('agent/other') === 1)
    expect(gateway!.unsubscribedRefs()).toEqual(['agent/route'])
    gateway!.conversationFrame('agent/other', [prompt(2, 'Second ask'), entry(3, 'Second thread')])
    await until(() => paneText().includes('Second thread'))
    expect(paneText()).not.toContain('First thread')

    mountPane('agent/route')
    await until(() => gateway!.subscribesFor('agent/route') === 2)
    gateway!.conversationFrame('agent/route', [prompt(1, 'First ask'), entry(2, 'First thread')])
    await until(() => paneText().includes('First thread'))
  })

  it('does not double-acquire a conversation the pane already holds when selection clicks it', async () => {
    open()
    mountPane('agent/route')
    await until(() => gateway!.subscribesFor('agent/route') === 1)
    live!.selectConversation('agent/route')
    await settle()
    expect(gateway!.subscribesFor('agent/route')).toBe(1)
    expect(gateway!.unsubscribedRefs()).toEqual([])
  })

  it('classifies a not-found read by its code, shows one fixed reason, and its recovery action re-acquires the follow', async () => {
    open()
    mountPane('agent/route')
    await until(() => gateway!.subscribesFor('agent/route') === 1)
    const sentinel = 'raw-read-diagnostic-sentinel'
    gateway!.send({
      kind: 'error', id: gateway!.conversationSubscription('agent/route').id, collection: 'conversation',
      code: 'not-found', message: sentinel, retryable: false,
    })
    await until(() => container.querySelector('[data-wf-unavailable="not-found"]') !== null)
    expect(container.querySelectorAll('[data-testid="transcript-unavailable"]')).toHaveLength(1)
    expect(container.querySelector('[data-wf-unavailable-code="not-found"]')).not.toBeNull()
    expect(paneText()).toContain('Conversation not found')
    expect(container.innerHTML).not.toContain(sentinel)
    expect(container.innerHTML).not.toContain('Unknown')
    // The kit renders the host's onRetrySync; today in its sync line, after the kit change as 'Try again'.
    const recovery = [...container.querySelectorAll<HTMLButtonElement>('[data-testid="transcript-unavailable"] button')]
      .filter(button => button.getAttribute('aria-label') === 'Retry loading conversation' || button.textContent === 'Try again')
    expect(recovery).toHaveLength(1)
    expect(recovery[0]!.disabled).toBe(false)
    flushSync(() => recovery[0]!.click())
    await until(() => gateway!.subscribesFor('agent/route') === 2)
    gateway!.conversationFrame('agent/route', [prompt(1, 'Recovered ask'), entry(2, 'Recovered thread')])
    await until(() => paneText().includes('Recovered thread'))
    expect(container.querySelector('[data-wf-unavailable]')).toBeNull()
  })
})
