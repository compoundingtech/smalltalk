import { readFileSync } from 'node:fs'
import { renderToStaticMarkup } from 'react-dom/server'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { ConversationItem } from '../conversation/model.ts'
import type { ConversationPage, Feed } from '../data/source.ts'
import type { FeedSyncObservation } from '../data/feedSync.ts'

// Node rendering exercises the real kit composition, not the CSS compiler.
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
  conversation: vi.fn(),
  conversationSync: vi.fn(),
  feedInterest: vi.fn(),
  retryConversation: vi.fn(),
}))
vi.mock('../data/react.tsx', () => ({
  useConversation: (ref: string) => { source.conversation(ref); return source.feed },
  useConversationSync: (ref: string) => { source.conversationSync(ref); return source.sync },
  useDataSource: () => ({ conversationInterest: undefined, retryConversation: source.retryConversation }),
  useFeedInterest: source.feedInterest,
  useNow: () => 1000,
}))

import { ConversationPane } from './ConversationPane.tsx'

const render = (ref = 'agent/selected') => renderToStaticMarkup(
  <ConversationPane agentRef={ref} agentName="Example Agent" onOpenTool={() => {}} />,
)
const observed = (items: readonly ConversationItem[] = [], hasOlder = false): Feed<ConversationPage> => ({
  _tag: 'Observed', freshness: 'live', value: { items, hasOlder, observation: { empty: items.length === 0 } },
})
const at = (seconds: number) => `2026-10-08T12:00:${String(seconds).padStart(2, '0').slice(-2)}.000Z`
/** The scenario native page the composition gate replays: markdown, reasoning, tools, custom events, truncation. */
const scenario: readonly ConversationItem[] = [
  { _tag: 'Text', id: 'p1', role: 'user', text: 'Keep the row projection readable.', attachments: [], streaming: false, at: at(0) },
  { _tag: 'ToolCall', id: 'read', callId: 'c1', name: 'read', input: { path: 'src/rows.ts' }, status: 'success', callSeen: true, result: { content: 'export const rows = []', mediaType: 'text/typescript', isError: false, at: at(3) }, at: at(1) },
  { _tag: 'Reasoning', id: 'reasoning', text: 'Compare the observed selection first.', streaming: false, durationMs: 120, at: at(4) },
  { _tag: 'Text', id: 'answer', role: 'assistant', text: 'The projection keeps **visible rows** together.', attachments: [], streaming: false, at: at(5) },
  { _tag: 'Status', id: 'status', status: 'completed', at: at(7) },
  { _tag: 'UnknownEvent', id: 'custom', eventType: 'custom', data: { raw: { type: 'custom', customType: 'tool_execution_start' } }, at: at(8) },
]

describe('ConversationPane kit composition', () => {
  beforeEach(() => {
    source.feed = observed()
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    source.conversation.mockClear()
    source.conversationSync.mockClear()
    source.feedInterest.mockClear()
  })

  it('renders the gated composition transcript for the selected follow, with no composer', () => {
    const html = render()
    expect(source.conversation).toHaveBeenCalledWith('agent/selected')
    expect(source.conversationSync).toHaveBeenCalledWith('agent/selected')
    expect(source.feedInterest).toHaveBeenCalledWith({ interest: undefined, visible: true })
    expect(html).toContain('aria-label="Transcript"')
    expect(html).toContain('data-testid="transcript-scroll"')
    expect(html).toContain('No messages yet')
    expect(html).not.toContain('textarea')
  })

  it('groups a scenario page into turns with rendered markdown and the reasoning disclosure', () => {
    // Complete history: the work fold settles and carries the elapsed summary.
    source.feed = observed(scenario.slice(0, 5))
    const html = render()
    expect(html.match(/data-testid="transcript-turn"/g)).toHaveLength(1)
    expect(html).toContain('data-testid="user-message"')
    expect(html).toContain('data-testid="agent-message"')
    expect(html).toContain('<strong>visible rows</strong>')
    expect(html).not.toContain('**visible rows**')
    // The reasoning disclosure lives inside the work fold; the jsdom composition
    // test opens it. Statically it must simply stay out of the lane — no leaked prefix.
    expect(html).not.toContain('[reasoning]')
    expect(html).not.toContain('Compare the observed selection first.')
    expect(html).toContain('data-testid="work-log"')
    expect(html).toContain('Worked for 7s')
  })

  it('omits known internal event kinds and repeats no truncation wording', () => {
    source.feed = observed(scenario, true)
    const html = render()
    expect(html).not.toContain('custom')
    expect(html).not.toContain('credential_pin')
    expect(html).not.toContain('title_change')
    expect(html).not.toContain('sequences')
    expect(html).not.toContain('Older history unavailable')
    expect(html).toContain('data-testid="history-boundary"')
    expect(html).toContain('Earlier messages not loaded')
  })

  it('uses the kit skeleton for the cold first-observation wait, not left-flush text', () => {
    source.feed = { _tag: 'Waiting' }
    source.sync = { status: { _tag: 'Requested', since: 990 }, observedAt: 990 }
    const html = render()
    expect(html).toContain('data-testid="transcript-placeholder"')
    expect(html).toContain('aria-label="Loading conversation"')
    expect(html).not.toContain('Waiting for the first conversation observation')
    expect(html).not.toContain('data-testid="transcript-turn"')
  })

  it('renders the kit unavailable state without the transcript lane', () => {
    source.feed = { _tag: 'Unavailable', reason: 'ungranted', detail: 'gateway 403 at /internal/read-diagnostic' }
    const html = render()
    expect(html).toContain('data-testid="transcript-unavailable"')
    expect(html).toContain('Conversation access not granted')
    expect(html).toContain('Ask the gateway owner to grant access to this surface. Your saved work is unchanged.')
    expect(html).not.toContain('read-diagnostic')
    expect(html).not.toContain('data-testid="transcript-scroll"')
  })

  /** A not-found read as live delivers it: fixed classification, raw message only in diagnostics. */
  const notFound = () => {
    const sentinel = 'raw-read-diagnostic-sentinel'
    source.feed = { _tag: 'Unavailable', reason: 'failed', detail: `${sentinel} /internal/path`, code: 'not-found' }
    source.sync = { status: { _tag: 'Failed', cause: { _tag: 'Server', code: 'not-found', message: sentinel } }, observedAt: 500 }
    return { html: render(), sentinel }
  }

  it('renders one unavailable state with a fixed not-found reason and diagnostics only in data-wf attributes', () => {
    const { html, sentinel } = notFound()
    expect(html.match(/data-testid="transcript-unavailable"/g)).toHaveLength(1)
    expect(html).toContain('Conversation not found')
    expect(html).toContain('There is no conversation for this agent right now.')
    expect(html).toContain('Ask the gateway owner')
    expect(html).not.toContain('Conversation unavailable</p>')
    expect(html).toContain('data-wf-unavailable="not-found"')
    expect(html).toContain('data-wf-unavailable-code="not-found"')
    // No raw text anywhere in the markup, which includes every aria-label and accessible name.
    expect(html).not.toContain(sentinel)
    expect(html).not.toContain('/internal/path')
    expect(html).not.toContain('Unknown')
    expect(html).not.toContain('data-testid="transcript-scroll"')
  })

  it('omits the data-wf code attribute for a code with spaces or markup', () => {
    source.feed = { _tag: 'Unavailable', reason: 'failed', detail: 'diagnostic', code: 'not found"><b>raw</b>' }
    const html = render()
    expect(html).toContain('data-wf-unavailable="failed"')
    expect(html).not.toContain('data-wf-unavailable-code')
    expect(html).not.toContain('raw')
    expect(html).toContain('Conversation unavailable')
  })
  it('keeps rendering the transcript when the sync status is not Live, with the honest SyncLine', () => {
    source.sync = { status: { _tag: 'Failed', cause: { _tag: 'Server', code: 'forbidden', message: 'Denied' } }, observedAt: 500 }
    source.feed = { _tag: 'Observed', freshness: 'stale', value: { items: scenario.slice(0, 5), hasOlder: false, observation: { empty: false } } }
    const html = render()
    expect(html).toContain('No access to conversation')
    expect(html).toContain('data-testid="transcript-turn"')
    expect(html).toContain('data-testid="agent-message"')
    expect(html).not.toContain('data-testid="transcript-placeholder"')
  })

  it('routes a filtered-empty page through the kit empty state', () => {
    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: [scenario[5]!], hasOlder: false, observation: { empty: false } } }
    const html = render()
    expect(html).toContain('data-testid="transcript-empty"')
    expect(html).toContain('This page contains no displayable conversation entries.')
  })

  it('renders the kit SyncLine failure and retains observed transcript content', () => {
    source.sync = { status: { _tag: 'Failed', cause: { _tag: 'Server', code: 'forbidden', message: 'Denied' } }, observedAt: 500 }
    source.feed = { ...observed(), freshness: 'stale', _tag: 'Observed', value: { items: [], hasOlder: false } }
    const html = render()
    expect(html).toContain('No access to conversation')
    expect(html).toContain('data-testid="transcript-scroll"')
  })

  it('adds only a layout-neutral diagnostics wrapper, no styling, and is mounted by the workspace', () => {
    const pane = readFileSync(new URL('./ConversationPane.tsx', import.meta.url), 'utf8')
    expect(pane).not.toMatch(/stylex|className/)
    // The only host element is a layout-neutral wrapper that carries the data-wf-* diagnostics.
    expect(pane.match(/<[a-z][a-z\d]*(?:\s|>)/g)).toEqual(['<div '])
    expect(pane).toContain("style={{ display: 'contents' }}")
    const workspace = readFileSync(new URL('./LiveAgentWorkspace.tsx', import.meta.url), 'utf8')
    expect(workspace).toContain('<ConversationPane key={current} agentRef={current} agentName={agentName} onOpenTool={setOpenedTool} />')
    expect(workspace).not.toContain('This subject has no available native view.')
  })
})

describe('single unavailable state', () => {
  it('does not repeat the failure in a header sync line while the pane shows the unavailable state', () => {
    source.feed = { _tag: 'Unavailable', reason: 'failed', detail: 'diagnostic', code: 'not-found' }
    source.sync = { status: { _tag: 'Failed', cause: { _tag: 'Server', code: 'not-found', message: 'diagnostic' } }, observedAt: 500 }
    const html = render()
    expect(html).not.toContain('data-testid="sync-line"')
    expect(html).not.toContain("Couldn't load conversation")
    expect(html).toContain('Conversation not found')
  })

  it('offers one recovery action in the unavailable body and no second error surface', () => {
    source.feed = { _tag: 'Unavailable', reason: 'failed', detail: 'diagnostic', code: 'not-found' }
    source.sync = { status: { _tag: 'Failed', cause: { _tag: 'Server', code: 'not-found', message: 'diagnostic' } }, observedAt: 500 }
    const html = render()
    const unavailable = html.slice(html.indexOf('data-testid="transcript-unavailable"'))
    expect(unavailable.match(/Try again/g)).toHaveLength(1)
    expect(html).not.toContain('role="alert"')
  })
})
