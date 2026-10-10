// @vitest-environment jsdom
/**
 * The activation bar for the transcript binding: a scenario native conversation page —
 * markdown, reasoning, a joined tool call and result, an unsupported custom event and a
 * truncation marker — flows through the pane's mapping into the gated kit composition, and the
 * rendered transcript reproduces the Storybook-reference structure on real components.
 */
import * as React from 'react'
import { useAui } from '@assistant-ui/react'
import { createRoot, type Root } from 'react-dom/client'
import { act } from 'react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ConversationItem } from '../conversation/model.ts'
import type { ConversationPage, Feed } from '../data/source.ts'
import type { FeedSyncObservation } from '../data/feedSync.ts'
import { Markdown, type WorkLogCall } from '@smalltalk/fractal-ui/assistant-ui'
import type * as Kit from '@smalltalk/fractal-ui/assistant-ui'
import type * as KitRuntime from '../../../../packages/fractal-ui/src/assistant-ui/EmbraceRuntime.tsx'
import type * as KitTranscript from '../../../../packages/fractal-ui/src/assistant-ui/composition/Transcript.tsx'
import type * as KitComposer from '../../../../packages/fractal-ui/src/assistant-ui/EmbraceComposer.tsx'
import type * as KitViewport from '../../../../packages/fractal-ui/src/assistant-ui/EmbraceScrollViewport.tsx'
import type { ConversationRuntimeOptions, TranscriptTurn } from '@smalltalk/fractal-ui/assistant-ui'
import { Tracer } from 'effect'
import { makeUxTelemetry, type UxTelemetry } from '../telemetry/ux.ts'

// The kit compiles StyleX at build time; node tests stub only the CSS runtime, never data hooks.
vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}),
  keyframes: () => 'test-animation',
  props: () => ({}),
}))

const source = vi.hoisted(() => ({
  feed: { _tag: 'Waiting' } as Feed<ConversationPage>,
  sync: undefined as FeedSyncObservation | undefined,
  now: 1000,
  sendEnabled: false,
  staticWorkLog: false,
  runtimeItems: [] as NonNullable<ConversationRuntimeOptions['messages']>[],
  transcriptTurns: [] as (readonly TranscriptTurn[])[],
  scrollToBottomKeys: [] as (string | undefined)[],
  composerProps: [] as React.ComponentProps<typeof Kit.EmbraceComposer>[],
  setDraft: undefined as ((text: string) => void) | undefined,
  viewportCommits: [] as {
    readonly anchorHistory: React.ComponentProps<typeof KitViewport.EmbraceScrollViewport>['anchorHistory']
    readonly rowIds: readonly string[]
    readonly renderedIds: readonly string[]
  }[],
}))

vi.mock('../data/react.tsx', async () => {
  // Vitest hoists this factory before static imports are initialized.
  const Atom = await import('effect/reactivity/Atom')
  const agents = Atom.make({ _tag: 'Observed' as const, freshness: 'live' as const, value: [] })
  return ({
  useConversation: () => source.feed,
  useConversationSync: () => source.sync,
  useDataSource: () => ({ agents, conversationInterest: undefined, attachments: source.sendEnabled ? { send: vi.fn() } : undefined }),
  useFeedInterest: () => {},
  useNow: () => source.now,
  useGrants: () => ({ actions: 'ungranted', messageSend: source.sendEnabled ? 'granted' : 'ungranted', terminalInput: 'ungranted' }),
  })
})

// Tap the input seams but keep the real kit/runtime rendering and effects.
vi.mock('../../../../packages/fractal-ui/src/assistant-ui/EmbraceRuntime.tsx', async importOriginal => {
  const kit = await importOriginal<typeof KitRuntime>()
  const DraftProbe = () => {
    const aui = useAui()
    source.setDraft = text => aui.composer().setText(text)
    return null
  }
  return {
    ...kit,
    EmbraceRuntimeProvider: (props: React.ComponentProps<typeof Kit.EmbraceRuntimeProvider>) => {
      source.runtimeItems.push(props.options.messages ?? [])
      return <kit.EmbraceRuntimeProvider {...props}><DraftProbe />{props.children}</kit.EmbraceRuntimeProvider>
    },
  }
})
vi.mock('../../../../packages/fractal-ui/src/assistant-ui/composition/Transcript.tsx', async importOriginal => {
  const kit = await importOriginal<typeof KitTranscript>()
  return {
    ...kit,
    Transcript: (props: React.ComponentProps<typeof Kit.Transcript>) => {
      source.transcriptTurns.push(props.turns)
      source.scrollToBottomKeys.push(props.scrollToBottomKey)
      // Exercise the kit's read-only presentation without weakening the host's required navigation API.
      return <kit.Transcript {...props} onOpenTool={source.staticWorkLog ? undefined : props.onOpenTool} />
    },
  }
})
vi.mock('../../../../packages/fractal-ui/src/assistant-ui/EmbraceComposer.tsx', async importOriginal => {
  const kit = await importOriginal<typeof KitComposer>()
  return {
    ...kit,
    EmbraceComposer: (props: React.ComponentProps<typeof Kit.EmbraceComposer>) => {
      source.composerProps.push(props)
      return <kit.EmbraceComposer {...props} />
    },
  }
})
vi.mock('../../../../packages/fractal-ui/src/assistant-ui/EmbraceScrollViewport.tsx', async importOriginal => {
  const kit = await importOriginal<typeof KitViewport>()
  return {
    ...kit,
    EmbraceScrollViewport: (props: React.ComponentProps<typeof KitViewport.EmbraceScrollViewport>) => {
      React.useLayoutEffect(() => {
        const lane = container.querySelector('[data-testid="transcript-scroll"]')
        source.viewportCommits.push({
          anchorHistory: props.anchorHistory,
          rowIds: props.items.map(row => row.id),
          renderedIds: Array.from(lane?.querySelectorAll('[data-item-id], [data-scroll-anchor-id]') ?? [],
            row => row.getAttribute('data-item-id') ?? row.getAttribute('data-scroll-anchor-id')).filter((id): id is string => id !== null),
        })
      })
      return <kit.EmbraceScrollViewport {...props} />
    },
  }
})

import { ConversationPane } from './ConversationPane.tsx'
import { WorkspaceBody } from './LiveAgentWorkspace.tsx'

const at = (seconds: number) => `2026-10-08T12:00:${String(seconds).padStart(2, '0').slice(-2)}.000Z`
/** The recorded scenario page: every item kind the gate replays, in delivery order. */
const scenario: readonly ConversationItem[] = [
  { _tag: 'Text', id: 'p1', role: 'user', text: 'Keep the row projection readable and verify the change.', attachments: [], streaming: false, at: at(0) },
  { _tag: 'ToolCall', id: 'read', callId: 'c1', name: 'read', input: { path: 'src/rows.ts' }, status: 'success', callSeen: true, result: { content: 'export const rows = []\nexport const count = rows.length', mediaType: 'text/typescript', isError: false, at: at(3) }, at: at(1) },
  { _tag: 'Reasoning', id: 'reasoning', text: 'Compare the observed selection with the projected rows.', streaming: false, durationMs: 1200, at: at(4) },
  { _tag: 'Text', id: 'answer', role: 'assistant', text: 'The projection keeps **visible rows** together and preserves selection.', attachments: [], streaming: false, at: at(6) },
  { _tag: 'Status', id: 'status', status: 'completed', detail: 'Finished', at: at(7) },
  { _tag: 'UnknownEvent', id: 'custom', eventType: 'custom', data: { raw: { type: 'custom', customType: 'tool_execution_start' } }, at: at(8) },
]

let root: Root | undefined
const container = document.createElement('div')

beforeEach(() => {
  source.now = 1000
  source.sendEnabled = false
  source.staticWorkLog = false
  source.runtimeItems = []
  source.transcriptTurns = []
  source.scrollToBottomKeys = []
  source.composerProps = []
  source.viewportCommits = []
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  // jsdom has no layout observers; the kit instantiates one while attaching scroll.
  // Test-environment sizing (react-aria renders every row) needs no measured entries.
  vi.stubGlobal('ResizeObserver', class {
    observe() {}
    unobserve() {}
    disconnect() {}
  })
  vi.stubGlobal('requestAnimationFrame', () => 1)
  vi.stubGlobal('cancelAnimationFrame', () => {})
  document.body.appendChild(container)
  root = createRoot(container)
})

afterEach(async () => {
  await act(async () => root?.unmount())
  container.remove()
  container.textContent = ''
  vi.unstubAllGlobals()
})

const opened: WorkLogCall[] = []

const mount = async (ux?: UxTelemetry) => {
  await act(async () => {
    root!.render(<ConversationPane agentRef="agent/selected" agentName="Example Agent" onOpenTool={call => opened.push(call)} ux={ux} />)
  })
}

const text = () => container.textContent ?? ''

describe('ConversationPane composition activation', () => {
  it('waits for actual source adoption without painting stranded rows or replacing the composer', async () => {
    source.sendEnabled = true
    source.feed = { _tag: 'Waiting' }
    source.sync = { status: { _tag: 'Requested', since: 990 }, observedAt: 990 }
    await mount()
    expect(container.querySelector('[data-testid="transcript-placeholder"]')?.textContent).toBe('')
    expect(container.querySelector('[data-testid="transcript-history-slot"]')).not.toBeNull()
    const strandedPaints: Element[] = []
    const observer = new MutationObserver(records => {
      for (const record of records) for (const node of record.addedNodes) if (node instanceof Element) {
        if (node.matches('[data-testid="transcript-stranded"]')) strandedPaints.push(node)
        strandedPaints.push(...node.querySelectorAll('[data-testid="transcript-stranded"]'))
      }
    })
    observer.observe(container, { childList: true, subtree: true })
    const loadingInput = container.querySelector('textarea')
    await act(async () => { source.setDraft!('Retain this unsent draft') })
    loadingInput!.setSelectionRange(7, 11)
    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: scenario, hasOlder: true, observation: { empty: false } } }
    source.sync = { status: { _tag: 'Live', since: 1000 }, observedAt: 1000 }
    try { await mount() } finally { observer.disconnect() }
    expect(strandedPaints).toHaveLength(0)
    const readyInput = container.querySelector('textarea')
    expect(readyInput).toBe(loadingInput)
    expect(readyInput?.value).toBe('Retain this unsent draft')
    expect(readyInput?.selectionStart).toBe(7)
    expect(readyInput?.selectionEnd).toBe(11)
    expect(container.querySelector('[data-testid="transcript-placeholder"]')).toBeNull()
    expect(container.querySelector('[data-testid="history-boundary"]')).not.toBeNull()
    readyInput?.focus()
    source.feed = { ...source.feed, value: { ...source.feed.value, items: [...scenario] } }
    await mount()
    expect(container.querySelector('textarea')).toBe(readyInput)
    expect(document.activeElement).toBe(readyInput)
    expect(readyInput?.selectionStart).toBe(7)
    expect(readyInput?.selectionEnd).toBe(11)
  })

  it('keeps an overflowing visible suffix stable instead of growing it at idle, and finds older rows on demand', async () => {
    const rows: ConversationItem[] = Array.from({ length: 30 }, (_, index) => ({
      _tag: 'Text', id: `long-${index}`, role: 'assistant', text: `History row ${index}`, attachments: [], streaming: false, at: at(index),
    }))
    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: rows, hasOlder: false, observation: { empty: false } } }
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    const height = vi.spyOn(HTMLElement.prototype, 'scrollHeight', 'get').mockImplementation(function (this: HTMLElement) {
      return this.dataset.testid === 'transcript-scroll' ? 1000 : 0
    })
    const client = vi.spyOn(HTMLElement.prototype, 'clientHeight', 'get').mockReturnValue(700)
    const bounds = vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function (this: HTMLElement) {
      return new DOMRect(0, 0, 700, this.dataset.testid === 'transcript-turn' ? 500 : 700)
    })
    const idle = vi.fn((_task: () => void) => 1)
    vi.stubGlobal('requestIdleCallback', idle)
    vi.stubGlobal('cancelIdleCallback', () => {})
    vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => setTimeout(() => callback(0), 0))
    try {
      await mount()
      await act(async () => {
        const { promise, resolve } = Promise.withResolvers<void>()
        setTimeout(resolve, 25)
        await promise
      })
      expect(text()).not.toContain('History row 0')
      expect(text()).toContain('History row 29')
      const before = container.querySelectorAll('[data-testid="transcript-message"]').length
      await act(async () => { idle.mock.calls[0]?.[0]() })
      expect(container.querySelectorAll('[data-testid="transcript-message"]')).toHaveLength(before)
      expect(idle).not.toHaveBeenCalled()
      const lane = container.querySelector<HTMLElement>('[data-testid="transcript-scroll"]')!
      expect(lane.dataset.followState).toBe('attached')
      await act(async () => { lane.dispatchEvent(new Event('scroll')) })
      expect(container.querySelectorAll('[data-testid="transcript-message"]')).toHaveLength(before)
      await act(async () => { window.dispatchEvent(new KeyboardEvent('keydown', { key: 'f', ctrlKey: true })) })
      expect(text()).toContain('History row 0')
    } finally { height.mockRestore(); client.mockRestore(); bounds.mockRestore() }
  })

  it.each([false, true])('groups all eleven reasoning entries of a work log into one disclosure (tool between=%s)', async separated => {
    const reasoning: ConversationItem[] = Array.from({ length: 11 }, (_, index) => ({
      _tag: 'Reasoning', id: `thought-${index}`, text: `Thought number ${index + 1}.`, streaming: false, at: at(index + 1),
    }))
    const items = separated ? [...reasoning.slice(0, 5), scenario[1]!, ...reasoning.slice(5)] : reasoning
    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: [scenario[0]!, ...items, scenario[3]!], hasOlder: false, observation: { empty: false } } }
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    // Native find exposes the complete history before asserting the full-turn grouping.
    vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => setTimeout(() => callback(0), 0))
    await mount()
    await act(async () => { window.dispatchEvent(new KeyboardEvent('keydown', { key: 'f', ctrlKey: true })) })
    await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="work-log"] button')!.click() })
    // Grouping is asserted on the complete turn, including the older reasoning prefix.
    await vi.waitFor(() => expect(container.querySelectorAll('[data-testid="thinking-entry"] button')).toHaveLength(1), { timeout: 5000 })
    const disclosures = [...container.querySelectorAll<HTMLButtonElement>('[data-testid="thinking-entry"] button')]
    expect(disclosures).toHaveLength(1)
    expect(disclosures.every(button => button.getAttribute('aria-expanded') === 'false')).toBe(true)
    expect(text()).not.toContain('Thought number 1.')
    for (const button of disclosures) await act(async () => { button.click() })
    expect([...container.querySelectorAll('[data-testid="thinking-entry"] [data-conversation-entry-id]')].map(element => element.textContent))
      .toEqual(reasoning.map(item => item._tag === 'Reasoning' ? item.text : ''))
  })

  it('insets the composer dock so its focus outline stays inside the viewport', async () => {
    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: scenario, hasOlder: false, observation: { empty: false } } }
    await mount()
    const dock = container.querySelector<HTMLElement>('[data-testid="conversation-composer-dock"]')
    expect(dock?.style.paddingBottom).toBe('12px')
    expect(dock?.style.flexShrink).toBe('0')
  })
  it('bounds the history lane and keeps inbound replies pinned only while following', async () => {
    let contentHeight = 2000
    const height = vi.spyOn(HTMLElement.prototype, 'scrollHeight', 'get').mockImplementation(() => contentHeight)
    const client = vi.spyOn(HTMLElement.prototype, 'clientHeight', 'get').mockReturnValue(400)
    const elementFromPoint = Object.getOwnPropertyDescriptor(document, 'elementFromPoint')
    Object.defineProperty(document, 'elementFromPoint', { configurable: true, value: () => null })
    const frames = new Map<number, FrameRequestCallback>()
    let nextFrame = 0
    vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
      frames.set(++nextFrame, callback)
      return nextFrame
    })
    vi.stubGlobal('cancelAnimationFrame', (id: number) => frames.delete(id))
    const flush = async () => {
      await act(async () => {
        const pending = [...frames.values()]
        frames.clear()
        pending.forEach(callback => callback(0))
      })
    }
    const reply = (id: string): ConversationItem => ({ _tag: 'Text', id, role: 'assistant', text: `Reply ${id}`, attachments: [], streaming: false, at: at(9) })
    const show = async (items: readonly ConversationItem[]) => {
      source.feed = { _tag: 'Observed', freshness: 'live', value: { items, hasOlder: false, observation: { empty: false } } }
      await mount()
      await flush()
    }
    try {
      await show(scenario)
      const host = container.querySelector<HTMLElement>('[data-testid="conversation-history-host"]')
      expect(host?.style.minHeight).toBe('0')
      expect(host?.style.flex).toBe('1 1 0px')
      const lane = container.querySelector<HTMLElement>('[data-testid="transcript-scroll"]')!
      expect(lane.scrollTop).toBe(1600)
      contentHeight = 2400
      await show([...scenario, reply('inbound-1')])
      expect(lane.scrollTop).toBe(2000)
      await act(async () => {
        lane.dispatchEvent(new WheelEvent('wheel', { deltaY: -900 }))
        lane.scrollTop = 700
        lane.dispatchEvent(new Event('scroll'))
      })
      await flush()
      contentHeight = 2800
      await show([...scenario, reply('inbound-1'), reply('inbound-2')])
      expect(lane.scrollTop).toBe(700)
    } finally {
      height.mockRestore()
      client.mockRestore()
      if (elementFromPoint === undefined) Reflect.deleteProperty(document, 'elementFromPoint')
      else Object.defineProperty(document, 'elementFromPoint', elementFromPoint)
    }
  })
  it('restores a detached conversation across switches and eviction, but reload follows the end', async () => {
    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: scenario, hasOlder: false, observation: { empty: false } } }
    // jsdom supplies no layout. Give the real kit controller a deterministic scroll lane;
    // all scroll, detach, save and restore behavior still runs in the actual components.
    const height = vi.spyOn(HTMLElement.prototype, 'scrollHeight', 'get').mockReturnValue(2000)
    const client = vi.spyOn(HTMLElement.prototype, 'clientHeight', 'get').mockReturnValue(400)
    const frames = new Map<number, FrameRequestCallback>()
    let nextFrame = 0
    vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
      frames.set(++nextFrame, callback)
      return nextFrame
    })
    vi.stubGlobal('cancelAnimationFrame', (id: number) => frames.delete(id))
    // No visible row geometry: the controller preserves the reading coordinate directly.
    const elementFromPoint = Object.getOwnPropertyDescriptor(document, 'elementFromPoint')
    Object.defineProperty(document, 'elementFromPoint', { configurable: true, value: () => null })
    const flushFrames = async () => {
      await act(async () => {
        const pending = [...frames.values()]
        frames.clear()
        for (const callback of pending) callback(0)
      })
    }
    let rosterRefs = ['agent/A', 'agent/B', 'agent/C', 'agent/D']
    const show = async (ref: string, surfaceKey = 'surface') => {
      await act(async () => root!.render(<WorkspaceBody key={surfaceKey} current={ref} rosterRefs={rosterRefs} view={{ _tag: 'Thread' }} agentName={ref} onOpenTool={() => {}} />))
      await flushFrames()
    }
    const lane = (ref: string) => {
      const title = [...container.querySelectorAll('[data-testid="transcript-scroll"]')]
        .find(node => node.textContent?.includes(ref))
      expect(title).toBeInstanceOf(HTMLElement)
      return title as HTMLElement
    }
    try {
      await show('agent/A')
      expect(lane('agent/A').scrollTop).toBe(1600)
      await act(async () => {
        const a = lane('agent/A')
        a.dispatchEvent(new WheelEvent('wheel', { deltaY: -900 }))
        a.scrollTop = 700
        a.dispatchEvent(new Event('scroll'))
      })
      await flushFrames()
      await show('agent/B')
      expect(lane('agent/B').scrollTop).toBe(1600)
      await show('agent/A')
      expect(lane('agent/A').scrollTop).toBe(700)
      // Retained DOM makes the short switch cheap. Eviction must not discard the
      // surface's reading memory when the fourth conversation remounts that pane.
      await show('agent/B')
      await show('agent/C')
      await show('agent/D')
      await show('agent/A')
      expect(lane('agent/A').scrollTop).toBe(700)
      await show('agent/A', 'reloaded-surface')
      expect(lane('agent/A').scrollTop).toBe(1600)
      await act(async () => {
        const a = lane('agent/A')
        a.dispatchEvent(new WheelEvent('wheel', { deltaY: -900 }))
        a.scrollTop = 700
        a.dispatchEvent(new Event('scroll'))
      })
      await flushFrames()
      await show('agent/B', 'reloaded-surface')
      await show('agent/C', 'reloaded-surface')
      await show('agent/D', 'reloaded-surface')
      // A leaves the roster after its DOM was evicted: remove only A's memory.
      rosterRefs = ['agent/B', 'agent/C', 'agent/D']
      await show('agent/D', 'reloaded-surface')
      rosterRefs = ['agent/A', ...rosterRefs]
      await show('agent/A', 'reloaded-surface')
      expect(lane('agent/A').scrollTop).toBe(1600)
      await act(async () => {
        const a = lane('agent/A')
        a.dispatchEvent(new WheelEvent('wheel', { deltaY: -900 }))
        a.scrollTop = 700
        a.dispatchEvent(new Event('scroll'))
      })
      await flushFrames()
      // A huge roster must not turn surface memory into an unbounded history.
      const extraRefs = Array.from({ length: 32 }, (_, index) => `agent/extra-${index}`)
      rosterRefs = [...rosterRefs, ...extraRefs]
      for (const ref of extraRefs) await show(ref, 'reloaded-surface')
      await show('agent/A', 'reloaded-surface')
      expect(lane('agent/A').scrollTop).toBe(1600)
    } finally {
      height.mockRestore()
      client.mockRestore()
      if (elementFromPoint === undefined) Reflect.deleteProperty(document, 'elementFromPoint')
      else Object.defineProperty(document, 'elementFromPoint', elementFromPoint)
    }
  })

  it('opts the composer into the shared reading column without overriding its placeholder', async () => {
    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: scenario, hasOlder: false, observation: { empty: false } } }
    await mount()
    expect(source.composerProps.length).toBeGreaterThan(0)
    expect(source.composerProps.every(props => props.readingColumn === true)).toBe(true)
    expect(source.composerProps.every(props => props.placeholder === undefined)).toBe(true)
  })

  it('keeps the source-owned scroll command stable through settlement, echo and Retry until a new Send', async () => {
    const first: ConversationItem = {
      _tag: 'Text', id: 'pending/first', role: 'user', text: 'First send',
      attachments: [], streaming: false, at: at(9), sendState: { _tag: 'Pending' },
    }
    const second: ConversationItem = { ...first, id: 'pending/second', text: 'Second send', at: at(10) }
    const renderItems = async (items: readonly ConversationItem[], lastSendId?: string) => {
      source.feed = { _tag: 'Observed', freshness: 'live', value: { items, lastSendId, hasOlder: false, observation: { empty: false } } }
      await mount()
    }
    await renderItems(scenario)
    expect(source.scrollToBottomKeys.at(-1)).toBeUndefined()
    await renderItems([...scenario, first], first.id)
    expect(source.scrollToBottomKeys.at(-1)).toBe(first.id)
    await renderItems([...scenario, first, second], second.id)
    expect(source.scrollToBottomKeys.at(-1)).toBe(second.id)
    // Settling the newer send must not fall back to the older Pending row.
    await renderItems([...scenario, first, { ...second, sendState: { _tag: 'Failed', reason: 'failed', detail: 'Unavailable' } }], second.id)
    expect(source.scrollToBottomKeys.at(-1)).toBe(second.id)
    await renderItems([...scenario, first, second], second.id)
    expect(source.scrollToBottomKeys.at(-1)).toBe(second.id)
    await renderItems([...scenario, { ...first, sendState: { _tag: 'Sent' } }, { ...second, sendState: { _tag: 'Sent' } }], second.id)
    expect(source.scrollToBottomKeys.at(-1)).toBe(second.id)
    await renderItems([...scenario, { ...first, id: 'timeline-entry/echo' }], second.id)
    expect(source.scrollToBottomKeys.at(-1)).toBe(second.id)
    // Retrying an older outbox row still does not issue a new scroll command.
    await renderItems([...scenario, first], second.id)
    expect(source.scrollToBottomKeys.at(-1)).toBe(second.id)
    await renderItems([], second.id)
    expect(source.scrollToBottomKeys.at(-1)).toBe(second.id)
    const third = { ...second, id: 'pending/third' }
    await renderItems([third], third.id)
    expect(source.scrollToBottomKeys.at(-1)).toBe(third.id)
    const changes = source.scrollToBottomKeys.filter((key, index, keys) => index === 0 || key !== keys[index - 1])
    expect(changes).toEqual([undefined, first.id, second.id, third.id])
  })

  it('keeps runtime items and transcript turns stable for sixty idle clock, sync and no-op frame updates', async () => {
    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: scenario, hasOlder: false, observation: { empty: false } } }
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    await mount()
    for (let second = 1; second <= 60; second += 1) {
      source.now += 1000
      const prior: Feed<ConversationPage> = source.feed
      if (prior._tag !== 'Observed') throw new Error('Expected an observed idle thread')
      source.feed = { ...prior, value: {
        ...prior.value,
        // A replace decode delivers new objects, unlike a heartbeat/delta with no changed rows.
        items: second % 10 === 0 ? structuredClone(scenario) : prior.value.items,
      } }
      source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: source.now }
      await mount()
    }
    const itemChanges = new Set(source.runtimeItems).size - 1
    const turnChanges = new Set(source.transcriptTurns).size - 1
    console.log(`idle-reference-changes/60-updates items=${itemChanges} turns=${turnChanges}`)
    expect(itemChanges).toBe(0)
    expect(turnChanges).toBe(0)
    const idleItems = source.runtimeItems.at(-1)
    source.feed = { _tag: 'Observed', freshness: 'live', value: {
      items: scenario.map(item => item._tag === 'Text' && item.id === 'p1' ? { ...item, text: 'Revised prompt' } : item),
      hasOlder: false, observation: { empty: false },
    } }
    await mount()
    expect(source.runtimeItems.at(-1)).not.toBe(idleItems)
    expect(text()).toContain('Revised prompt')
  })

  it.each(['turns', 'empty'] as const)('closes a switch after %s commit, never on the loading placeholder', async kind => {
    const paints = new Set<() => void>()
    const ended: Tracer.Span[] = []
    const tracer = Tracer.make({ span: options => {
      const span = Tracer.nativeTracer.span(options)
      const end = span.end.bind(span)
      span.end = (at, exit) => { end(at, exit); ended.push(span) }
      return span
    } })
    const ux = makeUxTelemetry({ tracer: () => tracer, paint: callback => {
      paints.add(callback)
      return () => { paints.delete(callback) }
    } })
    const commits: { readonly turn: boolean; readonly empty: boolean; readonly message: string }[] = []
    const report = ux.transcriptCommitted
    vi.spyOn(ux, 'transcriptCommitted').mockImplementation((ref) => {
      // Inspect synchronously, before act drains the runtime's passive message adoption.
      commits.push({
        turn: container.querySelector('[data-testid="transcript-turn"]') !== null,
        empty: container.querySelector('[data-testid="transcript-empty"]') !== null,
        message: container.querySelector('[data-testid="agent-message"]')?.textContent ?? '',
      })
      return report(ref)
    })
    try {
      ux.beginSwitch({ ref: 'agent/selected', warm: false, slotCount: 1 })
      source.feed = { _tag: 'Waiting' }
      source.sync = { status: { _tag: 'Requested', since: 990 }, observedAt: 990 }
      await mount(ux)
      expect(container.querySelector('[data-testid="transcript-placeholder"]')).not.toBeNull()
      expect(paints.size).toBe(0)
      source.feed = { _tag: 'Observed', freshness: 'live', value: {
        items: kind === 'turns' ? scenario : [], hasOlder: false, observation: { empty: kind === 'empty' },
      } }
      source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
      await mount(ux)
      expect(container.querySelector('[data-testid="transcript-placeholder"]')).toBeNull()
      expect(container.querySelector(`[data-testid="transcript-${kind === 'turns' ? 'turn' : 'empty'}"]`)).not.toBeNull()
      expect(paints.size).toBe(1)
      expect(commits).toHaveLength(1)
      expect(commits[0]?.turn).toBe(kind === 'turns')
      expect(commits[0]?.empty).toBe(kind === 'empty')
      if (kind === 'turns') expect(commits[0]?.message).toContain('visible rows')
      expect(ended.find(span => span.name === 'wf.ux.switch')).toBeUndefined()
      for (const paint of paints) paint()
      expect(ended.find(span => span.name === 'wf.ux.switch')?.attributes.get('wf.ux.painted')).toBe(true)
    } finally {
      ux.dispose()
    }
  })

  it('reproduces the reference transcript for a scenario native page', async () => {
    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: scenario, hasOlder: true, observation: { empty: false } } }
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    await mount()

    // One prompt-owned turn: user bubble, folded work log, reasoning disclosure, markdown answer.
    expect(container.querySelectorAll('[data-testid="transcript-turn"]')).toHaveLength(1)
    const turn = container.querySelector('[data-testid="transcript-turn"]')!
    expect(turn.querySelector('[data-testid="user-message"]')?.textContent).toContain('Keep the row projection readable')
    expect(turn.querySelector('[data-testid="user-message"]')?.getAttribute('data-send-state')).toBe('sent')
    expect(turn.querySelector('[data-testid="work-log"]')).not.toBeNull()
    const thinking = turn.querySelector('[data-testid="thinking-entry"]') as HTMLElement
    expect(thinking.textContent).toContain('Thinking')
    expect(text()).not.toContain('[reasoning]')
    // Markdown is rendered by the kit, never shown raw.
    const answer = turn.querySelector('[data-testid="agent-message"]')!
    expect(answer.querySelector('strong')?.textContent).toBe('visible rows')
    expect(answer.textContent).not.toContain('**')

    // Only known internal event kinds are omitted; unlisted kinds retain the kit Unknown row.
    expect(text()).not.toContain('custom')
    expect(text()).not.toContain('credential_pin')
    expect(text()).not.toContain('title_change')

    // Native HasOlder owns the single quiet in-lane history row.
    const boundary = container.querySelector('[data-testid="history-boundary"]')!
    expect(boundary.textContent).toContain('Earlier messages not loaded')
    expect(text()).not.toContain('sequences')
    expect(text()).not.toContain('Older history unavailable')
    expect(container.querySelector('[data-testid="history-boundary"] button')).toBeNull()
  })

  it.each(['interactive', 'static'] as const)('gives source-rendered tool, thinking and %s work-summary rows persistable leaf ids', async presentation => {
    source.staticWorkLog = presentation === 'static'
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    const show = async (items: readonly ConversationItem[]) => {
      source.feed = { _tag: 'Observed', freshness: 'live', value: { items, hasOlder: false, observation: { empty: false } } }
      await act(async () => root!.render(<ConversationPane agentRef="agent/selected" agentName="Example Agent" onOpenTool={call => opened.push(call)} />))
    }
    await show(scenario.slice(0, 5))
    const turn = container.querySelector('[data-testid="transcript-turn"]')!
    const summary = turn.querySelector<HTMLElement>('[data-testid="work-log"] > button, [data-testid="work-log"] > div')!
    expect(summary.getAttribute('data-scroll-anchor-id')).toBe('["work-summary","p1"]')
    if (presentation === 'interactive') await act(async () => { summary.click() })
    const tool = turn.querySelector('[data-tool-status]')!
    const thinking = turn.querySelector('[data-testid="thinking-entry"]')!
    expect(tool.getAttribute('data-item-id')).toBe('read')
    expect(thinking.getAttribute('data-scroll-anchor-id')).toBe('["thinking-summary","p1"]')
    expect(tool.querySelector('[data-testid="tool-detail-preview"]')?.closest('[data-item-id]')).toBe(tool)
    await act(async () => { thinking.querySelector<HTMLButtonElement>('button')!.click() })
    expect(thinking.textContent).toContain('Compare the observed selection')
    const reasoning = thinking.querySelector('[data-item-id="reasoning"]')!
    expect(thinking.querySelector('p')?.closest('[data-item-id]')).toBe(reasoning)

    // Older source turns can land above the work log without changing any nested row's identity.
    const older: readonly ConversationItem[] = [
      { _tag: 'Text', id: 'older/prompt', role: 'user', text: 'Earlier prompt', attachments: [], streaming: false, at: at(0) },
      { _tag: 'Text', id: 'older/answer', role: 'assistant', text: 'Earlier answer', attachments: [], streaming: false, at: at(0) },
    ]
    await show([...older, ...scenario.slice(0, 5)])
    expect(container.querySelector('[data-item-id="read"][data-tool-status]')).toBe(tool)
    expect(container.querySelector('[data-testid="thinking-entry"][data-scroll-anchor-id]')).toBe(thinking)
    expect(container.querySelector('[data-item-id="reasoning"][data-conversation-entry-id]')).toBe(reasoning)
    expect(summary.getAttribute('data-scroll-anchor-id')).toBe('["work-summary","p1"]')
    if (presentation === 'interactive') {
      await act(async () => { summary.click() })
      expect(turn.querySelector('[data-tool-status]')).toBeNull()
      expect(turn.querySelector('[data-testid="thinking-entry"]')).toBeNull()
      expect(summary.getAttribute('data-scroll-anchor-id')).toBe('["work-summary","p1"]')
    }
  })

  it('keeps full source anchor membership before rendered-prefix adoption and bounded reveal', async () => {
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    const items = Array.from({ length: 12 }, (_, index) => scenario.slice(0, 5).map(item =>
      item._tag === 'ToolCall' ? { ...item, id: `turn/${index}/${item.id}`, callId: `call/${index}` } : { ...item, id: `turn/${index}/${item.id}` })).flat()
    const show = async (messages: readonly ConversationItem[]) => {
      source.feed = { _tag: 'Observed', freshness: 'live', value: { items: messages, hasOlder: false, observation: { empty: false } } }
      await mount()
    }
    await show(items)
    // The bounded branch mounts six source rows, not six complete turns.
    expect(container.querySelectorAll('[data-testid="transcript-turn"]')).toHaveLength(2)
    expect(container.querySelector('[data-testid="user-message"][data-item-id="turn/0/p1"]')).toBeNull()
    const newest = container.querySelector('[data-testid="transcript-turn"][data-item-id="turn/11/p1"]')!
    await act(async () => { newest.querySelector<HTMLButtonElement>('[data-testid="work-log"] > button')!.click() })
    const suffix: readonly ConversationItem[] = [
      { _tag: 'ToolCall', id: 'suffix/tool', callId: 'suffix/call', name: 'read', input: {}, status: 'success', callSeen: true, result: { content: 'New output', isError: false, at: at(10) }, at: at(9) },
      { _tag: 'Reasoning', id: 'suffix/thinking', text: 'New reasoning', streaming: false, at: at(11) },
      { _tag: 'Text', id: 'suffix/answer', role: 'assistant', text: 'New answer', attachments: [], streaming: false, at: at(12) },
    ]
    source.viewportCommits = []
    await show([...items, ...suffix])
    const expectedIds = new Set([
      ...items.map(item => item.id), ...suffix.map(item => item.id),
      ...Array.from({ length: 12 }, (_, index) => JSON.stringify(['work-summary', `turn/${index}/p1`])),
      ...Array.from({ length: 12 }, (_, index) => JSON.stringify(['thinking-summary', `turn/${index}/p1`])),
    ])
    const pending = source.viewportCommits.find(commit => commit.renderedIds.includes('turn/11/answer') && !commit.renderedIds.includes('suffix/answer'))
    expect(pending).toBeDefined()
    expect(pending?.anchorHistory?._tag).toBe('Complete')
    if (pending?.anchorHistory?._tag !== 'Complete') throw new Error('Expected authoritative source membership before adoption')
    expect(new Set(pending.anchorHistory.ids)).toEqual(expectedIds)
    expect(pending.renderedIds).not.toContain('suffix/tool')
    expect(pending.renderedIds).not.toContain('suffix/thinking')

    const bounded = source.viewportCommits.at(-1)!
    expect(bounded.rowIds).toContain('turn/0/p1')
    expect(bounded.renderedIds).not.toContain('turn/0/p1')
    expect(bounded.anchorHistory?._tag).toBe('Complete')
    if (bounded.anchorHistory?._tag !== 'Complete') throw new Error('Expected authoritative source membership before bounded reveal')
    expect(new Set(bounded.anchorHistory.ids)).toEqual(expectedIds)
    expect(bounded.anchorHistory.ids).not.toContain('missing/source-row')
    expect(container.querySelector('[data-tool-status][data-item-id="suffix/tool"]')).not.toBeNull()
    expect(container.querySelector('[data-testid="thinking-entry"][data-scroll-anchor-id]')).not.toBeNull()
    expect(container.querySelector('[data-testid="agent-message"][data-item-id="suffix/answer"]')).not.toBeNull()

    await act(async () => { window.dispatchEvent(new KeyboardEvent('keydown', { key: 'f', ctrlKey: true })) })
    expect(container.querySelectorAll('[data-testid="transcript-turn"]')).toHaveLength(12)
    expect(container.querySelector('[data-testid="user-message"][data-item-id="turn/0/p1"]')).not.toBeNull()
    expect(source.viewportCommits.at(-1)?.anchorHistory).toBe(bounded.anchorHistory)
  })

  it('distinguishes partial or unobserved history from complete source membership and authoritative empty history', async () => {
    source.feed = { _tag: 'Waiting' }
    source.sync = { status: { _tag: 'Requested', since: 990 }, observedAt: 990 }
    await mount()
    expect(source.viewportCommits.at(-1)?.anchorHistory).toEqual({ _tag: 'Partial' })

    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: [], hasOlder: false, observation: { empty: true } } }
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    await mount()
    expect(source.viewportCommits.at(-1)?.anchorHistory).toEqual({ _tag: 'Complete', ids: [] })
    source.sync = { status: { _tag: 'Requested', since: 990 }, observedAt: 990 }
    await mount()
    expect(source.viewportCommits.at(-1)?.anchorHistory).toEqual({ _tag: 'Partial' })

    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: scenario.slice(0, 5), hasOlder: false, observation: { empty: false } } }
    await mount()
    expect(source.viewportCommits.at(-1)?.anchorHistory?._tag).toBe('Complete')
    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: scenario.slice(0, 5), hasOlder: true, observation: { empty: false } } }
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    await mount()
    expect(source.viewportCommits.at(-1)?.anchorHistory).toEqual({ _tag: 'Partial' })

    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: [scenario[0]!], hasOlder: false, observation: { empty: false } } }
    await mount()
    const removed = source.viewportCommits.at(-1)?.anchorHistory
    expect(removed?._tag).toBe('Complete')
    if (removed?._tag !== 'Complete') throw new Error('Expected complete source membership after removal')
    expect([...new Set(removed.ids)]).toEqual(['p1'])
  })

  it('hands an opened tool call to the host surface from the kit work log', async () => {
    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: scenario.slice(0, 5), hasOlder: false, observation: { empty: false } } }
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    await mount()

    const fold = container.querySelector<HTMLButtonElement>('[data-testid="work-log"] button')
    expect(fold?.textContent).toContain('Worked for 7s')
    await act(async () => { fold!.click() })
    const open = await act(async () => {
      const button = container.querySelector<HTMLButtonElement>('button[aria-label="Open Reading information tool detail"]')
      button?.click()
      return button
    })
    expect(open).not.toBeNull()
    expect(opened).toHaveLength(1)
    expect(opened[0]).toMatchObject({ id: 'read', kind: 'read', title: 'Reading information', argsSummary: undefined })
  })

  it('shows native text-block tool output on the expanded row instead of No output', async () => {
    // Native harness results arrive as content blocks, not a bare string (external_sessions.rs).
    const output = 'export const rows = []\nexport const count = rows.length'
    const blocks: ConversationItem = { _tag: 'ToolCall', id: 'read', callId: 'c1', name: 'read', input: { path: 'src/rows.ts' }, status: 'success', callSeen: true, at: at(1),
      result: { content: [{ type: 'text', text: output }], isError: false, at: at(3) } }
    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: [scenario[0]!, blocks, ...scenario.slice(2, 5)], hasOlder: false, observation: { empty: false } } }
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    await mount()

    const fold = container.querySelector<HTMLButtonElement>('[data-testid="work-log"] button')
    await act(async () => { fold!.click() })
    const log = container.querySelector('[data-testid="work-log"]')!
    expect(log.querySelector('[data-tool-status="success"]')?.textContent).not.toContain('No output')
    expect(log.querySelector('[data-testid="tool-detail-preview"]')?.textContent).toContain('export const rows = []')
  })

  it('keeps optimistic send state visible on its prompt', async () => {
    const pending: ConversationItem = { _tag: 'Text', id: 'p2', role: 'user', text: 'And verify the fix.', attachments: [], streaming: false, at: at(10), sendState: { _tag: 'Pending' } }
    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: [...scenario, pending], hasOlder: false, observation: { empty: false } } }
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    await mount()
    const prompts = container.querySelectorAll('[data-testid="user-message"]')
    expect(prompts).toHaveLength(2)
    expect(prompts[1]!.getAttribute('data-send-state')).toBe('pending')
  })

  it('renders unfamiliar protocol neutrally and hides known internal accounting', async () => {
    source.feed = { _tag: 'Observed', freshness: 'live', value: {
      items: [
        scenario[0]!,
        { _tag: 'UnknownEvent', id: 'accounting', eventType: 'custom/model_usage', data: { raw: { tokens: 42 } } },
        { _tag: 'UnknownEvent', id: 'unfamiliar', eventType: 'unfamiliar_kind', data: { raw: { payload: 'synthetic-payload-sentinel' } } },
      ],
      hasOlder: false, observation: { empty: false },
    } }
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    await mount()
    expect(text()).not.toContain('An event this view cannot show yet.')
    expect(text()).not.toContain('Unsupported event')
    expect(text()).not.toContain('custom/model_usage')
    expect(text()).not.toContain('unfamiliar_kind')
    expect(text()).not.toContain('synthetic-payload-sentinel')
    expect(container.querySelectorAll('[data-testid="transcript-message"]')).toHaveLength(0)
  })

  it.each(['running', 'completed'] as const)('renders no unavailable cancel control or raw state text while %s', async status => {
    source.feed = { _tag: 'Observed', freshness: 'live', value: {
      items: [scenario[0]!, { _tag: 'Status', id: 'run-status', status, at: at(1) }],
      hasOlder: false, observation: { empty: false },
    } }
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    await mount()
    expect(container.querySelector('textarea')).not.toBeNull()
    expect(text()).not.toContain('Cancel:')
    expect(text()).not.toContain('no cancel capability')
    expect(container.querySelector('button[aria-label*="Cancel"]')).toBeNull()
  })
  it('waits for the first observation inside the lane with the kit skeleton', async () => {
    source.feed = { _tag: 'Waiting' }
    source.sync = { status: { _tag: 'Requested', since: 990 }, observedAt: 990 }
    await mount()
    expect(container.querySelector('[data-testid="transcript-placeholder"]')).not.toBeNull()
    expect(text()).not.toContain('Waiting for the first conversation observation')
  })

  it('hands remote image consent to a new-tab opener without rendering a remote image', async () => {
    const image = 'https://images.example.invalid/preview.png'
    const open = vi.spyOn(window, 'open').mockImplementation(() => null)
    try {
      source.feed = { _tag: 'Observed', freshness: 'live', value: {
        items: [{ _tag: 'Text', id: 'image-answer', role: 'assistant', text: `![Preview](${image})`, attachments: [], streaming: false, at: at(6) }],
        hasOlder: false, observation: { empty: false },
      } }
      source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
      await mount()
      const placeholder = container.querySelector('[data-testid="deferred-image"]')
      expect(placeholder?.textContent).toContain('images.example.invalid')
      expect(container.querySelector(`img[src="${image}"]`)).toBeNull()
      expect(open).not.toHaveBeenCalled()
      const button = placeholder?.querySelector<HTMLButtonElement>('button')
      expect(button?.textContent).toBe('Open image · images.example.invalid')
      await act(async () => { button!.click() })
      expect(open).toHaveBeenCalledExactlyOnceWith(image, '_blank', 'noopener,noreferrer')
      expect(container.querySelector(`img[src="${image}"]`)).toBeNull()
    } finally {
      open.mockRestore()
    }
  })

  it('negative control: without the host opener, kit consent loads the image inline', async () => {
    const image = 'https://images.example.invalid/control.png'
    await act(async () => { root!.render(<Markdown text={`![Control](${image})`} />) })
    expect(container.querySelector(`img[src="${image}"]`)).toBeNull()
    const button = container.querySelector<HTMLButtonElement>('[data-testid="deferred-image"] button')
    expect(button?.textContent).toBe('Load image')
    await act(async () => { button!.click() })
    expect(container.querySelector(`img[src="${image}"]`)).not.toBeNull()
  })

  it('renders an assistant-only page without a synthetic user bubble', async () => {
    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: [scenario[3]!, scenario[4]!], hasOlder: false, observation: { empty: false } } }
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    await mount()
    expect(container.querySelector('[data-testid="agent-message"]')).not.toBeNull()
    expect(container.querySelector('[data-testid="user-message"]')).toBeNull()
  })
})
