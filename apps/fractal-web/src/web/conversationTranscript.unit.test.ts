import { describe, expect, it } from 'vitest'
import { convertConversationItem } from '../../../../packages/fractal-ui/src/assistant-ui/embrace-converter.ts'
import type { SyncStatus } from '@smalltalk/fractal-ui/assistant-ui/sync'
import type { ConversationItem } from '../conversation/model.ts'
import { mapConversationFeed, mapConversationItemsForTranscript, mapConversationSync, transcriptRuntimeOptions } from './conversationTranscript.ts'

const at = '2026-10-08T12:00:00.000Z'
const sender = { kind: 'human', label: 'Reader' } as const
const items = [
  { _tag: 'Text', id: 'text', role: 'user', text: 'Hello', streaming: false, attachments: [{ id: 'attachment', mediaType: 'text/plain' }], at, sender },
  { _tag: 'Message', id: 'message', messageId: 'envelope', from: 'agent/example', title: 'Delivery', at },
  { _tag: 'Reasoning', id: 'reasoning', text: 'Considering', streaming: true, durationMs: 120, at },
  { _tag: 'ToolCall', id: 'tool', callId: 'call', name: 'read', input: { path: 'example.ts' }, status: 'success', callSeen: true, result: { content: 'Result', isError: false, at }, at },
  { _tag: 'Status', id: 'status', status: 'completed', detail: 'Finished', at },
  { _tag: 'Usage', id: 'usage', semantics: 'response', inputTokens: 12, outputTokens: 34, cost: 0.01, currency: 'USD', at },
  { _tag: 'Notice', id: 'notice', kind: 'redaction', text: 'Hidden', detail: 'Private content', at },
  { _tag: 'Event', id: 'event', kind: 'harness-message', title: 'Notification', text: 'Observed', sender, data: { value: 1 }, at },
  { _tag: 'UnknownEvent', id: 'unknown', eventType: 'future-event', data: { value: 2 }, at },
] satisfies readonly ConversationItem[]

const statuses: readonly SyncStatus[] = [
  { _tag: 'Connecting', attempt: 2, since: 100 },
  { _tag: 'Requested', since: 100 },
  ...(['queued', 'resolving', 'routing', 'reading'] as const).map(stage => ({ _tag: 'Progress' as const, stage, elapsedMs: 50, stageSince: 100, reportedAt: 150, done: 1, total: 3 })),
  { _tag: 'Live', since: 150, snapshot: { revision: '1' } },
  { _tag: 'Stale', reason: { _tag: 'Resync', code: 'gap', message: 'Missing frame', attempt: 2 }, lastLiveAt: 90 },
  { _tag: 'Stale', reason: { _tag: 'Quiet', lastFrameAt: 90 } },
  { _tag: 'Stale', reason: { _tag: 'Reconnecting', attempt: 2, nextAt: 300, issue: 'Disconnected' } },
  { _tag: 'Stale', reason: { _tag: 'Evicted' } },
  { _tag: 'Stale', reason: { _tag: 'Unknown' } },
  { _tag: 'Failed', cause: { _tag: 'Server', code: 'forbidden', message: 'Denied' } },
  { _tag: 'Failed', cause: { _tag: 'Local', kind: 'decode', detail: { message: 'Invalid frame', cap: 3 } } },
  { _tag: 'Failed', cause: { _tag: 'Unknown' } },
]

describe('kit transcript mapping', () => {
  it.each(items)('preserves $_tag and delegates its presentation to the kit converter', item => {
    const mapped = mapConversationItemsForTranscript([item])
    expect(mapped[0]).toBe(item)
    const message = convertConversationItem(mapped[0]!)
    expect(message.id).toBe(item.id)
    expect(message.metadata?.custom?.item).toBe(item)
    expect(message.content.length).toBeGreaterThan(0)
  })

  it('passes the snapshot by reference and exposes no mutation capabilities', async () => {
    expect(mapConversationItemsForTranscript(items)).toBe(items)
    const runtime = transcriptRuntimeOptions(items)
    expect(runtime.messages).toBe(items)
    expect(runtime.isDisabled).toBe(true)
    expect(runtime.onEdit).toBeUndefined()
    expect(runtime.onReload).toBeUndefined()
    expect(runtime.onCancel).toBeUndefined()
    await expect(runtime.onNew({
      role: 'user', content: [{ type: 'text', text: 'No send' }], createdAt: new Date(at),
      metadata: { custom: {} }, parentId: null, sourceId: null, runConfig: undefined,
    })).rejects.toThrow('read-only')
  })

  it.each(statuses)('preserves $_tag sync facts, including nested reasons', status => {
    const mapped = mapConversationSync({ status, observedAt: 123 }, 456)
    expect(mapped).toEqual({ status, observedAt: 123, now: 456, label: 'conversation' })
    expect(mapped?.status).toBe(status)
  })

  it('does not manufacture a sync verdict without an observed frame', () => {
    expect(mapConversationSync(undefined, 456)).toBeUndefined()
  })

  it.each(['ungranted', 'unsupported', 'failed'] as const)('maps Unavailable(%s) without claiming emptiness', reason => {
    expect(mapConversationFeed({ _tag: 'Unavailable', reason, detail: 'Actual diagnostic' })).toMatchObject({ _tag: 'Unavailable', detail: 'Actual diagnostic' })
  })

  it('distinguishes waiting, observed empty, filtered empty, older history, and retained failures', () => {
    expect(mapConversationFeed({ _tag: 'Waiting' })).toEqual({ _tag: 'Waiting' })
    expect(mapConversationFeed({ _tag: 'Observed', freshness: 'live', value: { items: [], hasOlder: false, observation: { empty: true } } })).toEqual({ _tag: 'Observed', items: [], hasOlder: false, filteredEmpty: false })
    expect(mapConversationFeed({ _tag: 'Observed', freshness: 'live', value: { items: [], hasOlder: true, observation: { empty: false } } })).toMatchObject({ hasOlder: true, filteredEmpty: true })
    expect(mapConversationFeed({ _tag: 'Observed', freshness: 'stale', value: { items, hasOlder: true }, error: { reason: 'failed', detail: 'Read refused' } })).toMatchObject({ items, hasOlder: true, notice: 'Conversation unavailable: Read refused. Showing the last verified entries.' })
    expect(mapConversationFeed({ _tag: 'Observed', freshness: 'stale', value: { items, hasOlder: false } })).toMatchObject({ notice: 'Showing the last verified conversation entries.' })
  })
})
