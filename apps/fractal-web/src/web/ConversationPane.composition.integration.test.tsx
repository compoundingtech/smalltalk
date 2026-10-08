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
import type { WorkLogCall } from '@smalltalk/fractal-ui/assistant-ui'

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
  { _tag: 'UnknownEvent', id: 'custom', eventType: 'custom', data: { kind: 'custom' }, at: at(8) },
  { _tag: 'Notice', id: 'truncated', kind: 'truncation', text: 'Older history unavailable in this transcript window', detail: 'retained · sequences 0–40 omitted', at: at(9) },
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

    // Unsupported event kinds are omitted by structural kind; no card per unknown event.
    expect(text()).not.toContain('custom')
    expect(text()).not.toContain('credential_pin')
    expect(text()).not.toContain('title_change')

    // Truncation collapses into the single quiet in-lane history row.
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
      const button = container.querySelector('button[aria-label="Open read tool detail"]') as HTMLButtonElement | null
      button?.click()
      return button
    })
    expect(open).not.toBeNull()
    expect(opened).toHaveLength(1)
    expect(opened[0]).toMatchObject({ id: 'read', kind: 'read', title: 'read', argsSummary: 'src/rows.ts' })
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

  it('waits for the first observation inside the lane with the kit skeleton', async () => {
    source.feed = { _tag: 'Waiting' }
    source.sync = { status: { _tag: 'Requested', since: 990 }, observedAt: 990 }
    await mount()
    expect(container.querySelector('[data-testid="transcript-placeholder"]')).not.toBeNull()
    expect(text()).not.toContain('Waiting for the first conversation observation')
  })

  // Kit gap: TranscriptTurn.prompt is required (packages/fractal-ui/src/assistant-ui/composition/Transcript.tsx),
  // so an assistant-only page — a leading prompt-less tail — cannot be rendered yet. The mapping
  // omits that tail rather than synthesising a fake prompt; flip this on once the kit makes
  // prompt optional.
  it.skip('renders an assistant-only page once the kit accepts prompt-less turns', async () => {
    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: [scenario[3]!, scenario[4]!], hasOlder: false, observation: { empty: false } } }
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    await mount()
    expect(container.querySelector('[data-testid="agent-message"]')).not.toBeNull()
  })
})
