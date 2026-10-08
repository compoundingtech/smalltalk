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

const entry = (sequence: number, text: string): TimelineEntry => ({
  id: `timeline-entry/${sequence}`,
  sequence,
  revision: 1,
  type: 'content',
  role: 'assistant',
  final: true,
  timestamp: snapshot.created_at,
  body: { text, media_type: 'text/markdown' },
})

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

  conversationFrame(ref: string, rows: TimelineEntry[]) {
    this.send({
      kind: 'conversation',
      id: this.conversationSubscription(ref).id,
      collection: 'conversation',
      session_id: `session/${ref}`,
      items: rows,
      replace: true,
      has_more: false,
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
        <ConversationPane key={agentRef} agentRef={agentRef} />
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
    gateway!.conversationFrame('agent/route', [entry(1, 'Cold route hello')])
    await until(() => live!.registry.get(live!.source.sync!.conversation('agent/route')).sync.status._tag === 'Live')
    await until(() => paneText().includes('Cold route hello'))
    expect(gateway!.unsubscribedRefs()).toEqual([])
  })

  it('moves demand with the pane across back/forward agent switches', async () => {
    // One conversation slot: the warm pool cannot retain the released thread, so switching
    // demand must evict the previous agent's follow with a real unsubscribe.
    open(1)
    mountPane('agent/route')
    await until(() => gateway!.subscribesFor('agent/route') === 1)
    gateway!.conversationFrame('agent/route', [entry(1, 'First thread')])
    await until(() => paneText().includes('First thread'))

    mountPane('agent/other')
    await until(() => gateway!.subscribesFor('agent/other') === 1)
    expect(gateway!.unsubscribedRefs()).toEqual(['agent/route'])
    gateway!.conversationFrame('agent/other', [entry(2, 'Second thread')])
    await until(() => paneText().includes('Second thread'))
    expect(paneText()).not.toContain('First thread')

    mountPane('agent/route')
    await until(() => gateway!.subscribesFor('agent/route') === 2)
    gateway!.conversationFrame('agent/route', [entry(1, 'First thread')])
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
})
