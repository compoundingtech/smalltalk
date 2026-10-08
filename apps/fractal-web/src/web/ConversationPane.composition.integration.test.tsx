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
}))

vi.mock('../data/react.tsx', () => ({
  useConversation: () => source.feed,
  useConversationSync: () => source.sync,
  useDataSource: () => ({ conversationInterest: undefined }),
  useFeedInterest: () => {},
  useNow: () => 1000,
}))

import { ConversationPane } from './ConversationPane.tsx'

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

const mount = async () => {
  await act(async () => {
    root!.render(<ConversationPane agentRef="agent/selected" agentName="Example Agent" onOpenTool={call => opened.push(call)} />)
  })
}

const text = () => container.textContent ?? ''

describe('ConversationPane composition activation', () => {
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
