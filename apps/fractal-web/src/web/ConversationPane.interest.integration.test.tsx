// @vitest-environment jsdom
/**
 * The shell acquires route demand before pane code loads; standalone panes own their demand.
 * Cold mounting and switching agents follow through the real SDK and live source
 * (scripted gateway, real socket commands). Nothing here mocks the data hooks.
 */
import type { Agent, CollectionCommand, CollectionFrame, CollectionSocket, Snapshot, TimelineEntry } from '@smalltalk/st3-client'
import { webcrypto } from 'node:crypto'
import { setTimeout as yieldIo } from 'node:timers/promises'
import * as Native from '@smalltalk/st3-client/schema'
import { flushSync } from 'react-dom'
import type { ComponentProps } from 'react'
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
import type { UxTelemetry } from '../telemetry/ux.ts'

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
  grantSend = false
  failSend = false
  abortSend = false
  snapshotFailure: string | undefined
  sendGate: Promise<void> | undefined
  readonly actions: Native.ActionRequest[] = []
  echoId: string | undefined
  sendMessageId: string | undefined
  onUnsubscribe: (() => void) | undefined

  readonly fetch: typeof fetch = async (input, init) => {
    const path = new URL(String(input)).pathname
    if (path === '/v1/client/actions') {
      const action = Native.decodeUnknownSync(Native.ActionRequest)(JSON.parse(String(init?.body)))
      if (action.type !== 'message.send') throw new Error('Expected a message.send action')
      this.actions.push(action)
      const digest = new Uint8Array(await webcrypto.subtle.digest('SHA-256', new TextEncoder().encode(action.idempotency_key)))
      this.echoId = this.sendMessageId ?? `message/${[...digest.slice(0, 8)].map(byte => byte.toString(16).padStart(2, '0')).join('')}`
      await this.sendGate
      if (this.abortSend) throw new TypeError('Failed to fetch')
      if (this.failSend) return new Response(JSON.stringify({
        api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', code: 'unavailable',
        message: 'Forced send outage', retryable: true, request_id: 'request/forced', details: {},
      }), { status: 503, headers: { 'content-type': 'application/json' } })
      return new Response(JSON.stringify({ api_version: 'st3.client.v0', snapshot, value: {
        kind: 'action-result', action_id: action.id, operation_id: 'operation/send',
        status: 'completed', affected_ids: [this.echoId], snapshot_id: snapshot.id,
      } }), { headers: { 'content-type': 'application/json' } })
    }
    if (path !== '/v1/client/capabilities') throw new Error(`Unexpected request ${path}`)
    if (this.snapshotFailure !== undefined) throw new TypeError(this.snapshotFailure)
    return new Response(
      JSON.stringify({
        api_version: 'st3.client.v0',
        snapshot,
        value: {
          kind: 'capabilities',
          capabilities: [
            { id: 'work.done', state: 'granted', version: 0 },
            { id: 'message.send', state: this.grantSend ? 'granted' : 'ungranted', version: 0 },
          ],
          event_cursor: 'cursor/current',
          oldest_event_cursor: 'cursor/oldest',
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
      send: (text: string) => {
        const command: CollectionCommand = JSON.parse(text)
        this.commands.push(command)
        if (command.kind === 'unsubscribe') this.onUnsubscribe?.()
      },
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
  vi.stubGlobal('crypto', webcrypto)
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
  vi.restoreAllMocks()
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
  for (let round = 0; round < maxRounds && !check(); round += 1) { await settle(); await yieldIo(5) }
  if (!check()) throw new Error(`Condition not reached: ${container.textContent}; actions=${gateway?.actions.length}`)
}

const open = (conversationSlots: number | 'advertised' = 'advertised', grantSend = false) => {
  gateway = new Gateway()
  gateway.grantSend = grantSend
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
  it('follows before the pane import resolves, hands off one interest, and releases it on navigation and unmount', async () => {
    window.history.replaceState(null, '', '/w/agent/route')
    open(1)
    const interest = live!.source.conversationInterest!('agent/route')
    let handedInterest: typeof interest | undefined
    let resolveImport!: () => void
    const imported = new Promise<void>(resolve => { resolveImport = resolve })
    vi.doMock('./ConversationPane.tsx', async () => {
      await imported
      return {
        ConversationPane: (props: ComponentProps<typeof ConversationPane>) => {
          handedInterest = props.interest
          return <ConversationPane {...props} />
        },
      }
    })
    try {
      root = createRoot(container)
      flushSync(() => root!.render(
        <DataSourceProvider source={live!.source} registry={live!.registry}><LiveAgentWorkspace /></DataSourceProvider>,
      ))
      await settle()
      expect(container.querySelector('[aria-label="Loading conversation"]')).not.toBeNull()
      expect(gateway!.subscribesFor('agent/route')).toBe(1)
      resolveImport()
      gateway!.conversationFrame('agent/route', [prompt(1, 'Early prompt'), entry(2, 'Early conversation')])
      await until(() => paneText().includes('Early conversation'))
      expect(handedInterest).toBe(interest)
      expect(gateway!.subscribesFor('agent/route')).toBe(1)
      flushSync(() => {
        window.history.pushState(null, '', '/w/agent/other')
        window.dispatchEvent(new PopStateEvent('popstate'))
      })
      await until(() => gateway!.subscribesFor('agent/other') === 1)
      expect(gateway!.unsubscribedRefs()).toEqual(['agent/route'])
      flushSync(() => root!.unmount())
      root = undefined
      // With one slot, this follow can only proceed if the unmounted shell released
      // the last visible interest in the previous conversation.
      const release = live!.registry.mount(live!.source.conversationInterest!('agent/final'))
      try {
        await until(() => gateway!.subscribesFor('agent/final') === 1)
        expect(gateway!.unsubscribedRefs()).toEqual(['agent/route', 'agent/other'])
      } finally {
        release()
      }
    } finally {
      resolveImport()
      vi.doUnmock('./ConversationPane.tsx')
      window.history.replaceState(null, '', '/')
    }
  })

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
    const raw = [...container.querySelectorAll<HTMLButtonElement>('button')].find(button => button.textContent === 'Show raw input/output')!
    expect(raw).toBeDefined()
    flushSync(() => raw.click())
    const button = container.querySelector('button[aria-label="Open Reading information tool detail"]') as HTMLButtonElement
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
    expect(container.querySelector('[data-testid="sync-line"]')).toBeNull()
    const recovery = [...container.querySelectorAll<HTMLButtonElement>('[data-testid="transcript-unavailable"] button')]
      .filter(button => button.textContent === 'Try again')
    expect(recovery).toHaveLength(1)
    expect(recovery[0]!.disabled).toBe(false)
    flushSync(() => recovery[0]!.click())
    await until(() => gateway!.subscribesFor('agent/route') === 2)
    gateway!.conversationFrame('agent/route', [prompt(1, 'Recovered ask'), entry(2, 'Recovered thread')])
    await until(() => paneText().includes('Recovered thread'))
    expect(container.querySelector('[data-wf-unavailable]')).toBeNull()
  })
})

describe('LiveAgentWorkspace retained panes', () => {
  const committed: string[] = []
  const ux: UxTelemetry = {
    activeSpan: () => undefined,
    traceContext: () => undefined,
    shellCommitted: () => () => {},
    rosterCommitted: () => () => {},
    beginSwitch: () => {},
    switchDataReady: () => {},
    transcriptCommitted: (ref) => { committed.push(ref); return () => {} },
    beginSendEcho: () => ({ committed: () => () => {}, cancel: () => {} }),
    observeSync: () => {},
    dispose: () => {},
  }
  const mountWorkspace = (ref: string) => {
    committed.length = 0
    window.history.replaceState(null, '', `/w/${ref}`)
    root = createRoot(container)
    flushSync(() => root!.render(<DataSourceProvider source={live!.source} registry={live!.registry}><LiveAgentWorkspace ux={ux} /></DataSourceProvider>))
  }
  const go = (ref: string) => flushSync(() => {
    window.history.pushState(null, '', `/w/${ref}`)
    window.dispatchEvent(new PopStateEvent('popstate'))
  })
  const lanes = () => [...container.querySelectorAll('[data-testid="transcript-scroll"]')]
  const lane = (text: string) => lanes().find(node => node.textContent?.includes(text))
  afterEach(() => window.history.replaceState(null, '', '/'))

  it('paints a cold pane reselected during eviction, then reveals its retained DOM immediately', async () => {
    open(1)
    const waitForEvent = async (check: () => boolean) => {
      for (let round = 0; round < 400 && !check(); round += 1) await settle()
      expect(check()).toBe(true)
    }
    mountWorkspace('agent/route')
    await waitForEvent(() => gateway!.subscribesFor('agent/route') === 1)
    let reselected = false
    const switchedAt = performance.now()
    gateway!.onUnsubscribe = () => {
      if (reselected) return
      reselected = true
      go('agent/route')
    }
    go('agent/other')
    await waitForEvent(() => reselected)
    await waitForEvent(() => gateway!.subscribesFor('agent/route') === 2)
    const firstFrameAt = performance.now()
    gateway!.conversationFrame('agent/route', [prompt(1, 'Cold prompt'), entry(2, 'Recovered cold transcript')])
    await waitForEvent(() => committed.includes('agent/route'))
    const paintedAt = performance.now()
    const first = lane('Recovered cold transcript')!
    expect(first.querySelector('[data-testid="transcript-placeholder"]')).toBeNull()
    go('agent/other')
    const warmSwitchAt = performance.now()
    go('agent/route')
    expect(lane('Recovered cold transcript')).toBe(first)
    expect(committed.filter(ref => ref === 'agent/route')).toHaveLength(2)
    console.log(`conversation-switch-measurement ${JSON.stringify({
      switchToPaintMs: paintedAt - switchedAt,
      firstFrameToPaintMs: paintedAt - firstFrameAt,
      retainedSwitchToCommitMs: performance.now() - warmSwitchAt,
    })}`)
  })

  it('reveals a switched-away pane without remounting it or refollowing its warm thread', async () => {
    open()
    mountWorkspace('agent/route')
    await until(() => gateway!.subscribesFor('agent/route') === 1)
    gateway!.conversationFrame('agent/route', [prompt(1, 'First ask'), entry(2, 'First thread')])
    await until(() => lane('First thread') !== undefined)
    const first = lane('First thread')!
    expect(committed).toEqual(['agent/route'])

    go('agent/other')
    await until(() => gateway!.subscribesFor('agent/other') === 1)
    gateway!.conversationFrame('agent/other', [prompt(2, 'Second ask'), entry(3, 'Second thread')])
    await until(() => lane('Second thread') !== undefined)
    // Only the shown pane reports a transcript commit; the hidden one stays attached.
    expect(committed).toEqual(['agent/route', 'agent/other'])
    expect(first.isConnected).toBe(true)

    go('agent/route')
    await settle()
    expect(lane('First thread')).toBe(first)
    expect(committed).toEqual(['agent/route', 'agent/other', 'agent/route'])
    expect(gateway!.subscribesFor('agent/route')).toBe(1)
    expect(gateway!.unsubscribedRefs()).toEqual([])
  })

  it('releases a hidden pane follow so a single conversation slot can move to the shown agent', async () => {
    // A hidden pane that still held visible demand would pin the only slot: visible follows are
    // never evicted, so the second agent could not subscribe.
    open(1)
    mountWorkspace('agent/route')
    await until(() => gateway!.subscribesFor('agent/route') === 1)
    gateway!.conversationFrame('agent/route', [prompt(1, 'First ask'), entry(2, 'First thread')])
    await until(() => lane('First thread') !== undefined)
    const first = lane('First thread')!

    go('agent/other')
    await until(() => gateway!.subscribesFor('agent/other') === 1)
    expect(gateway!.unsubscribedRefs()).toEqual(['agent/route'])
    expect(first.isConnected).toBe(true)

    go('agent/route')
    await until(() => gateway!.subscribesFor('agent/route') === 2)
    expect(lanes()).toHaveLength(2)
  })

  it('locks and blurs the outgoing composer in the switch commit, then restores its retained draft', async () => {
    open('advertised', true)
    mountWorkspace('agent/route')
    await until(() => gateway!.subscribesFor('agent/route') === 1)
    gateway!.conversationFrame('agent/route', [])
    await until(() => container.querySelector('[data-testid="transcript-empty"]') !== null && container.querySelector('textarea:not(:disabled)') !== null)
    const input = container.querySelector('textarea')!
    const form = input.closest('form')!
    const send = vi.spyOn(live!.source.attachments!, 'send')
    const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!
    flushSync(() => {
      setter.call(input, 'Retained draft')
      input.dispatchEvent(new Event('input', { bubbles: true }))
      input.focus()
    })
    await settle()
    expect(document.activeElement).toBe(input)

    go('agent/other')
    // No switch frame has run yet: the outgoing pane is still covered, not layout-locked.
    expect(form.inert).toBe(true)
    expect(document.activeElement).not.toBe(input)
    expect(input.closest('[aria-hidden]')?.getAttribute('aria-hidden')).toBe('true')
    const enter = new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true })
    const click = new MouseEvent('click', { bubbles: true, cancelable: true })
    expect(input.dispatchEvent(enter)).toBe(false)
    expect(form.dispatchEvent(click)).toBe(false)
    expect(send).not.toHaveBeenCalled()
    await until(() => gateway!.subscribesFor('agent/other') === 1)

    go('agent/route')
    await settle()
    expect(container.querySelector('textarea')).toBe(input)
    expect(input.value).toBe('Retained draft')
    expect(form.inert).toBe(false)
    expect(input.closest('[aria-hidden]')?.getAttribute('aria-hidden')).toBe('false')
    expect(gateway!.subscribesFor('agent/route')).toBe(1)
    expect(send).not.toHaveBeenCalled()
  })

  it('bounds retained panes to the three most recently opened agents', async () => {
    open()
    mountWorkspace('agent/route')
    await until(() => gateway!.subscribesFor('agent/route') === 1)
    gateway!.conversationFrame('agent/route', [prompt(1, 'First ask'), entry(2, 'First thread')])
    await until(() => lane('First thread') !== undefined)
    const first = lane('First thread')!
    for (const ref of ['agent/two', 'agent/three']) {
      go(ref)
      await until(() => gateway!.subscribesFor(ref) === 1)
    }
    expect(lanes()).toHaveLength(3)
    expect(first.isConnected).toBe(true)
    go('agent/four')
    await until(() => gateway!.subscribesFor('agent/four') === 1)
    expect(lanes()).toHaveLength(3)
    expect(first.isConnected).toBe(false)
  })
})

const scratchSeat = 'agent/example/scratch'
const prepareSendPane = async (granted = true) => {
  open('advertised', granted)
  mountPane(scratchSeat)
  await until(() => gateway!.subscribesFor(scratchSeat) === 1)
  gateway!.conversationFrame(scratchSeat, [])
  await until(() => container.querySelector('textarea') !== null)
  await settle()
}
const submitDraft = async () => {
  const input = container.querySelector('textarea')
  if (input === null) throw new Error('Kit composer was not mounted')
  const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!
  flushSync(() => {
    setter.call(input, 'hello')
    input.dispatchEvent(new Event('input', { bubbles: true }))
  })
  await settle()
  const form = input.closest('form')
  if (form === null) throw new Error('Kit composer form was not mounted')
  flushSync(() => form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })))
  await settle()
}
const userRows = () => [...container.querySelectorAll('[data-testid="user-message"][data-send-state]')]

describe('ConversationPane composer/send binding', () => {
  it.each([
    { echoFirst: false, affectedId: undefined },
    { echoFirst: true, affectedId: undefined },
    { echoFirst: false, affectedId: 'message/gateway-assigned' },
  ])('keeps the own-send row and turn mounted through acknowledgement ($echoFirst, $affectedId)', async ({ echoFirst, affectedId }) => {
    // The real kit scroller runs against deterministic jsdom dimensions; browser replay
    // separately checks measured row geometry in a fully styled lane.
    vi.spyOn(HTMLElement.prototype, 'scrollHeight', 'get').mockReturnValue(2000)
    vi.spyOn(HTMLElement.prototype, 'clientHeight', 'get').mockReturnValue(400)
    await prepareSendPane()
    gateway!.sendMessageId = affectedId
    const send = vi.spyOn(live!.source.attachments!, 'send')
    let resolve!: () => void
    gateway!.sendGate = new Promise<void>(done => { resolve = done })
    await submitDraft()
    await until(() => gateway!.actions.length === 1)
    expect(send).toHaveBeenCalledTimes(1)
    expect(send.mock.calls[0]![0]).toMatchObject({ _tag: 'Send', parameters: { to: scratchSeat, content: 'hello' } })
    expect(userRows()).toHaveLength(1)
    expect(userRows()[0]!.getAttribute('data-send-state')).toBe('pending')
    const pendingRow = userRows()[0]!
    const itemId = pendingRow.getAttribute('data-item-id')
    const turn = pendingRow.closest('[data-testid="transcript-turn"]')
    expect(itemId).toBe(`pending/${gateway!.actions[0]!.idempotency_key}`)
    expect(turn?.getAttribute('data-item-id')).toBe(itemId)
    const lane = container.querySelector<HTMLElement>('[data-testid="transcript-scroll"]')!
    expect(lane.scrollTop).toBe(1600)
    if (!echoFirst) {
      resolve()
      await until(() => userRows()[0]?.getAttribute('data-send-state') === 'sent')
      expect(userRows()[0]).toBe(pendingRow)
    }
    await until(() => gateway!.echoId !== undefined)
    const echoId = gateway!.echoId
    if (echoId === undefined) throw new Error('the send action returned no message id to echo')
    gateway!.conversationFrame(scratchSeat, [
      { ...entry(1, ''), id: 'timeline-entry/echo/0123456789abcdef-message', type: 'message', role: 'user', body: { message_id: echoId, from: 'person/operator', to: scratchSeat } },
      { ...entry(2, 'Acknowledged hello'), id: 'timeline-entry/echo/0123456789abcdef-content', type: 'content', role: 'user', body: { text: 'Acknowledged hello', media_type: 'text/plain' } },
    ])
    await until(() => userRows()[0]?.textContent === 'Acknowledged hello')
    expect(userRows()[0]).toBe(pendingRow)
    expect(userRows()[0]!.getAttribute('data-item-id')).toBe(itemId)
    expect(userRows()[0]!.closest('[data-testid="transcript-turn"]')).toBe(turn)
    expect(turn?.getAttribute('data-item-id')).toBe(itemId)
    if (echoFirst) {
      resolve()
      await settle()
      expect(userRows()[0]).toBe(pendingRow)
      expect(userRows()[0]!.getAttribute('data-item-id')).toBe(itemId)
    }
    expect(lane.scrollTop).toBe(1600)
    expect(userRows()).toHaveLength(1)
    expect(send).toHaveBeenCalledTimes(1)
  })

  it('keeps the Sent row across a newer read that omits it (st #1977), with no resend', async () => {
    await prepareSendPane()
    const send = vi.spyOn(live!.source.attachments!, 'send')
    await submitDraft()
    await until(() => userRows()[0]?.getAttribute('data-send-state') === 'sent')
    // Reopen the source follow; the newest authoritative page still omits remotely owned mail.
    gateway!.socket!.onclose?.({ code: 1006, reason: 'reconnect proof' })
    await until(() => gateway!.subscribesFor(scratchSeat) === 2)
    gateway!.conversationFrame(scratchSeat, [])
    await settle()
    expect(userRows()).toHaveLength(1)
    expect(userRows()[0]!.textContent).toContain('hello')
    expect(userRows()[0]!.getAttribute('data-send-state')).toBe('sent')
    expect(send).toHaveBeenCalledTimes(1)
  })

  it('retries two network failures and then succeeds with exactly one action per retry and the original key', async () => {
    await prepareSendPane()
    const send = vi.spyOn(live!.source.attachments!, 'send')
    gateway!.abortSend = true
    await submitDraft()
    await until(() => userRows()[0]?.getAttribute('data-send-state') === 'failed')
    const firstKey = gateway!.actions[0]!.idempotency_key
    const rowId = userRows()[0]!.getAttribute('data-item-id')
    const sendRows = () => userRows().filter(row => row.getAttribute('data-item-id') === rowId)
    for (let attempt = 1; attempt <= 3; attempt += 1) {
      gateway!.conversationFrame(scratchSeat, [prompt(10, 'Previous prompt'), entry(11, 'Unrelated response')])
      live!.registry.refresh(live!.source.grants)
      await settle()
      const retry = [...container.querySelectorAll<HTMLButtonElement>('[data-testid="send-failure"] button')]
        .find(button => button.textContent === 'Retry')
      expect(retry).toBeDefined()
      expect(retry!.disabled).toBe(false)
      let resolve!: () => void
      gateway!.sendGate = new Promise<void>(done => { resolve = done })
      gateway!.abortSend = attempt < 3
      if (attempt === 2) {
        gateway!.socket!.onclose?.({ code: 1006, reason: 'retry freshness proof' })
        await until(() => gateway!.subscribesFor(scratchSeat) === 2)
        gateway!.conversationFrame(scratchSeat, [prompt(10, 'Previous prompt'), entry(11, 'New unrelated response')])
        await settle()
      }
      flushSync(() => {
        retry!.dispatchEvent(new MouseEvent('mousedown', { bubbles: true, button: 0, detail: 1 }))
        retry!.dispatchEvent(new MouseEvent('mouseup', { bubbles: true, button: 0, detail: 1 }))
        retry!.dispatchEvent(new MouseEvent('click', { bubbles: true, button: 0, detail: 1 }))
      })
      await until(() => gateway!.actions.length === attempt + 1)
      expect(send).toHaveBeenCalledTimes(attempt + 1)
      expect(send.mock.calls[attempt]![0]).toMatchObject({ _tag: 'Resend', idempotencyKey: firstKey })
      await until(() => sendRows()[0]?.getAttribute('data-send-state') === 'pending')
      expect(sendRows()[0]!.getAttribute('data-item-id')).toBe(rowId)
      resolve()
      await until(() => sendRows()[0]?.getAttribute('data-send-state') === (attempt < 3 ? 'failed' : 'sent'))
      expect(gateway!.actions.map(action => action.idempotency_key)).toEqual(Array(attempt + 1).fill(firstKey))
      expect(sendRows()).toHaveLength(1)
    }
  })

  it('keeps a server refusal on its row and retries it with the original key', async () => {
    await prepareSendPane()
    gateway!.failSend = true
    await submitDraft()
    await until(() => userRows()[0]?.getAttribute('data-send-state') === 'failed')
    const firstKey = gateway!.actions[0]!.idempotency_key
    const failure = container.querySelector('[data-testid="send-failure"]')!
    flushSync(() => failure.querySelector<HTMLButtonElement>('button[aria-expanded]')!.click())
    expect(failure.textContent).toContain('The message could not be sent. Check your connection and retry.')
    expect(container.querySelector('textarea')!.disabled).toBe(false)
    gateway!.failSend = false
    const retry = [...failure.querySelectorAll<HTMLButtonElement>('button')].find(button => button.textContent === 'Retry')!
    flushSync(() => retry.click())
    await until(() => userRows()[0]?.getAttribute('data-send-state') === 'sent')
    expect(gateway!.actions.map(action => action.idempotency_key)).toEqual([firstKey, firstKey])
    expect(userRows()).toHaveLength(1)
  })

  it('keeps network failure on the row, never in composer footer or status text', async () => {
    await prepareSendPane()
    gateway!.abortSend = true
    await submitDraft()
    await until(() => userRows()[0]?.getAttribute('data-send-state') === 'failed')
    await settle()
    expect(container.querySelector('[data-testid="send-failure"]')!.textContent).toContain("Couldn't send")
    const footer = container.querySelector('textarea')!.closest('form')!
    expect(footer.textContent).not.toMatch(/failed|fetch|couldn't send/i)
    expect(container.querySelector('textarea')!.disabled).toBe(false)
    const disclosure = container.querySelector<HTMLButtonElement>('[data-testid="send-failure"] button[aria-expanded]')!
    flushSync(() => disclosure.click())
    expect(paneText()).not.toContain('Failed to fetch')
    expect([...container.querySelectorAll('[role="status"]')].map(status => status.textContent).join(' '))
      .not.toMatch(/failed|fetch|couldn't send/i)
  })

  it('never exposes snapshot-acquisition diagnostics in rows, disclosures, footer, status or accessible names', async () => {
    await prepareSendPane()
    const sentinel = 'raw-snapshot-diagnostic-sentinel'
    gateway!.snapshotFailure = sentinel
    await submitDraft()
    await until(() => userRows()[0]?.getAttribute('data-send-state') === 'failed')
    const feed = live!.registry.get(live!.source.conversation(scratchSeat))
    expect(feed).toMatchObject({ _tag: 'Observed', value: { items: [{
      sendState: { _tag: 'Failed', reason: 'snapshot-unavailable', detail: expect.stringContaining(sentinel) },
    }] } })
    expect(gateway!.actions).toHaveLength(0)
    const failure = container.querySelector('[data-testid="send-failure"]')!
    flushSync(() => failure.querySelector<HTMLButtonElement>('button[aria-expanded]')!.click())
    expect(failure.textContent).not.toContain(sentinel)
    expect(userRows()[0]!.textContent).not.toContain(sentinel)
    expect(container.querySelector('textarea')!.closest('form')!.textContent).not.toContain(sentinel)
    expect([...container.querySelectorAll('[role="status"]')].map(status => status.textContent).join(' ')).not.toContain(sentinel)
    expect(container.innerHTML).not.toContain(sentinel)
  })

  it('disables the composer when only broad actions, not message send, are granted', async () => {
    await prepareSendPane(false)
    expect(container.querySelector('textarea')!.disabled).toBe(true)
    expect(paneText()).toContain('Message send is not granted')
    await submitDraft()
    expect(gateway!.actions).toHaveLength(0)
    expect(container.querySelector('[aria-label^="Cancel"]')).toBeNull()
    expect(paneText()).not.toContain('no cancel capability')
  })

  it('negative control: removing the send binding fails the submit-once criterion', async () => {
    await prepareSendPane()
    const send = vi.spyOn(live!.source.attachments!, 'send')
    // Remove the port so the same pane cannot issue a send. The positive criterion must reject it.
    const { attachments: _attachments, ...withoutSend } = live!.source
    flushSync(() => root!.render(<DataSourceProvider source={withoutSend} registry={live!.registry}>
      <ConversationPane agentRef={scratchSeat} agentName="Scratch" onOpenTool={vi.fn()} />
    </DataSourceProvider>))
    await submitDraft()
    expect(() => expect(send).toHaveBeenCalledTimes(1)).toThrow()
    expect(paneText()).toContain('This view cannot send messages.')
  })
})
