import { readFileSync } from 'node:fs'
import { renderToStaticMarkup } from 'react-dom/server'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { ConversationPage, Feed } from '../data/source.ts'
import type { FeedSyncObservation } from '../data/feedSync.ts'

// Node rendering exercises the real kit components, not the CSS compiler.
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
}))
vi.mock('../data/react.tsx', () => ({
  useConversation: (ref: string) => { source.conversation(ref); return source.feed },
  useConversationSync: (ref: string) => { source.conversationSync(ref); return source.sync },
  useDataSource: () => ({ conversationInterest: undefined }),
  useFeedInterest: source.feedInterest,
  useNow: () => 1000,
}))

import { ConversationPane } from './ConversationPane.tsx'

const render = (ref = 'agent/selected') => renderToStaticMarkup(<ConversationPane agentRef={ref} />)
const observed = (items: ConversationPage['items'] = [], hasOlder = false): Feed<ConversationPage> => ({
  _tag: 'Observed', freshness: 'live', value: { items, hasOlder, observation: { empty: items.length === 0 } },
})

describe('ConversationPane kit rendering', () => {
  beforeEach(() => {
    source.feed = observed()
    source.sync = { status: { _tag: 'Live', since: 100 }, observedAt: 100 }
    source.conversation.mockClear()
    source.conversationSync.mockClear()
    source.feedInterest.mockClear()
  })

  it('renders the real kit transcript for the selected follow, with no composer or live spinner', () => {
    const html = render()
    expect(source.conversation).toHaveBeenCalledWith('agent/selected')
    expect(source.conversationSync).toHaveBeenCalledWith('agent/selected')
    expect(source.feedInterest).toHaveBeenCalledWith({ interest: undefined, visible: true })
    expect(html).toContain('aria-label="Conversation"')
    expect(html).toContain('data-testid="transcript-scroll"')
    // A read-only pane keeps the kit's neutral empty copy; it never invites a send.
    expect(html).toContain('No messages yet')
    expect(html).not.toContain('textarea')
    expect(html).not.toContain('role="progressbar"')
    expect(html).not.toContain('Connecting')
  })

  it('renders actual messages through the kit rather than app-authored rows', () => {
    source.feed = observed([{ _tag: 'Text', id: 'reply', role: 'assistant', text: 'Verified reply', attachments: [], streaming: false, at: '2026-10-08T12:00:00.000Z' }])
    const html = render('agent/other')
    expect(source.conversation).toHaveBeenCalledWith('agent/other')
    expect(html).toContain('data-testid="transcript-message"')
    expect(html).toContain('Verified reply')
    expect(html).not.toContain('No messages yet')
  })

  it('uses kit boundaries for waiting, unavailable, older pages and filtered empty', () => {
    source.feed = { _tag: 'Waiting' }
    source.sync = undefined
    expect(render()).toContain('role="note"')
    expect(render()).not.toContain('No messages yet')
    source.feed = { _tag: 'Unavailable', reason: 'ungranted', detail: 'Read access was refused.' }
    expect(render()).toContain('Conversation access not granted')
    expect(render()).toContain('Read access was refused.')
    expect(render()).not.toContain('transcript-scroll')
    source.feed = observed([], true)
    expect(render()).toContain('Earlier conversation entries are not included in this page.')
    source.feed = { _tag: 'Observed', freshness: 'live', value: { items: [], hasOlder: false, observation: { empty: false } } }
    expect(render()).toContain('This page contains no displayable conversation entries.')
    expect(render()).not.toContain('No messages yet')
  })

  it('renders the real kit SyncLine failure and retains observed transcript content', () => {
    source.sync = { status: { _tag: 'Failed', cause: { _tag: 'Server', code: 'forbidden', message: 'Denied' } }, observedAt: 500 }
    source.feed = { ...observed(), freshness: 'stale', _tag: 'Observed', value: { items: [], hasOlder: false } }
    const html = render()
    expect(html).toContain('No access to conversation')
    expect(html).toContain('Showing the last verified conversation entries.')
    expect(html).toContain('transcript-scroll')
  })

  it('contains no app-authored DOM or styling and is mounted by the workspace', () => {
    const pane = readFileSync(new URL('./ConversationPane.tsx', import.meta.url), 'utf8')
    expect(pane).not.toMatch(/stylex|className|style=/)
    expect(pane).not.toMatch(/<[a-z][a-z\d]*(?:\s|>)/)
    const workspace = readFileSync(new URL('./LiveAgentWorkspace.tsx', import.meta.url), 'utf8')
    expect(workspace).toContain('<ConversationPane key={current} agentRef={current} />')
    expect(workspace).not.toContain('This subject has no available native view.')
  })
})
