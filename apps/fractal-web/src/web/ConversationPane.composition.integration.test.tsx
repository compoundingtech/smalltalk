// @vitest-environment jsdom
/**
 * The activation bar for the transcript binding: a scenario native conversation page —
 * markdown, reasoning, a joined tool call and result, an unsupported custom event and a
 * truncation marker — flows through the pane's mapping into the gated kit composition, and the
 * rendered transcript reproduces the Storybook-reference structure on real components.
 */
import * as React from 'react'
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
  runtimeItems: [] as NonNullable<ConversationRuntimeOptions['messages']>[],
  transcriptTurns: [] as (readonly TranscriptTurn[])[],
  scrollToBottomKeys: [] as (string | undefined)[],
  composerProps: [] as React.ComponentProps<typeof Kit.EmbraceComposer>[],
}))

vi.mock('../data/react.tsx', async () => {
  // Vitest hoists this factory before static imports are initialized.
  const Atom = await import('effect/reactivity/Atom')
  const agents = Atom.make({ _tag: 'Observed' as const, freshness: 'live' as const, value: [] })
  return ({
  useConversation: () => source.feed,
  useConversationSync: () => source.sync,
  useDataSource: () => ({ agents, conversationInterest: undefined }),
  useFeedInterest: () => {},
  useNow: () => source.now,
  useGrants: () => ({ actions: 'ungranted', messageSend: 'ungranted', terminalInput: 'ungranted' }),
  })
})

// Tap the input seams but keep the real kit/runtime rendering and effects.
vi.mock('../../../../packages/fractal-ui/src/assistant-ui/EmbraceRuntime.tsx', async importOriginal => {
  const kit = await importOriginal<typeof KitRuntime>()
  return {
    ...kit,
    EmbraceRuntimeProvider: (props: React.ComponentProps<typeof Kit.EmbraceRuntimeProvider>) => {
      source.runtimeItems.push(props.options.messages ?? [])
      return <kit.EmbraceRuntimeProvider {...props} />
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
      return <kit.Transcript {...props} />
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
  source.runtimeItems = []
  source.transcriptTurns = []
  source.scrollToBottomKeys = []
  source.composerProps = []
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

  it('hands an opened tool call to the host surface from the kit work log', async () => {
    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: scenario.slice(0, 5), hasOlder: false, observation: { empty: false } } }
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    await mount()

    const fold = container.querySelector<HTMLButtonElement>('[data-testid="work-log"] button')
    expect(fold?.textContent).toContain('Worked for 7s')
    await act(async () => { fold!.click() })
    const raw = [...container.querySelectorAll<HTMLButtonElement>('button')].find(button => button.textContent === 'Show raw input/output')
    await act(async () => { raw!.click() })
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
    const preview = log.querySelector('[data-testid="tool-detail-preview"]')!
    expect(preview.textContent).not.toContain(output)
    expect(preview.querySelector('pre')).toBeNull()
    const raw = [...preview.querySelectorAll<HTMLButtonElement>('button')].find(button => button.textContent === 'Show raw input/output')
    expect(raw?.getAttribute('aria-expanded')).toBe('false')
    await act(async () => { raw!.click() })
    expect(log.querySelector('[data-testid="tool-detail-preview"]')?.textContent).toContain('export const rows = []')
  })

  it('keeps failed command diagnostics behind the banner raw disclosure', async () => {
    const diagnostic = '/tmp/example/ready-step-test.fixture\nTraceback (most recent call last):\n  File "/tmp/example/check.py", line 4\nAssertionError: expected ready state'
    source.feed = { _tag: 'Observed', freshness: 'live', value: {
      items: [scenario[0]!, { _tag: 'ToolCall', id: 'failed-run', callId: 'failed-call', name: 'run',
        input: { command: 'python check.py', summary: 'Checking ready state' }, status: 'error', callSeen: true, at: at(1),
        result: { content: diagnostic, isError: true, at: at(3) } }],
      hasOlder: false, observation: { empty: false },
    } }
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    await mount()
    const banner = container.querySelector<HTMLElement>('[data-error-overlay]')!
    expect(banner.textContent).toContain('Command did not complete')
    expect(banner.textContent).toContain('AssertionError: expected ready state')
    expect(banner.textContent).not.toContain('/tmp/example')
    expect(banner.textContent).not.toContain('Traceback')
    const raw = [...banner.querySelectorAll<HTMLButtonElement>('button')].find(button => button.textContent === 'Show raw input/output')!
    expect(raw.getAttribute('aria-expanded')).toBe('false')
    await act(async () => { raw.click() })
    expect(banner.textContent).toContain(diagnostic)
  })

  it.each([
    ['/tmp/example/ready-step-test.fixture\nPermissionError: cannot read /tmp/example/check.py', 'PermissionError: cannot read …/check.py'],
    ['/tmp/example/ready-step-test.fixture', 'Tool failed; no readable reason was recorded.'],
  ])('uses a readable failed tool reason instead of a path tail: %s', async (diagnostic, reason) => {
    source.feed = { _tag: 'Observed', freshness: 'live', value: {
      items: [scenario[0]!, { _tag: 'ToolCall', id: 'failed-run', callId: 'failed-call', name: 'run',
        input: { command: 'check' }, status: 'error', callSeen: true, at: at(1),
        result: { content: diagnostic, isError: true, at: at(3) } }],
      hasOlder: false, observation: { empty: false },
    } }
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    await mount()
    const fold = container.querySelector<HTMLButtonElement>('[data-testid="work-log"] button')!
    await act(async () => { fold.click() })
    const row = container.querySelector<HTMLElement>('[data-tool-status="error"]')!
    const open = row.querySelector<HTMLButtonElement>('button')
    if (open !== null) await act(async () => { open.click() })
    expect(row.querySelector('[data-testid="tool-error-reason"]')?.textContent).toBe(reason)
    expect(row.querySelector('pre')).toBeNull()
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
