import { describe, expect, it } from 'vitest'
import { convertConversationItem } from '../../../../packages/fractal-ui/src/assistant-ui/embrace-converter.ts'
import type { SyncStatus } from '@smalltalk/fractal-ui/assistant-ui/sync'
import type { ConversationItem, SendState } from '../conversation/model.ts'
import { mapConversationFeed, transcriptRuntimeOptions, transcriptSyncStatus, transcriptTurnsForItems } from './conversationTranscript.ts'

const at = (seconds: number) => `2026-10-08T12:00:${String(seconds).padStart(2, '0').slice(-2)}.000Z`
const sender = { kind: 'human', label: 'Reader' } as const

const prompt = (id: string, text: string, seconds: number, sendState?: SendState): ConversationItem => ({
  _tag: 'Text', id, role: 'user', text, attachments: [], streaming: false, at: at(seconds), sender,
  ...(sendState === undefined ? {} : { sendState }),
})

/** A scenario native page: two prompt-owned turns around omitted kinds and a truncation marker. */
const scenario: readonly ConversationItem[] = [
  prompt('p1', 'Keep the row projection readable.', 0),
  { _tag: 'ToolCall', id: 'read', callId: 'c1', name: 'read', input: { path: 'src/rows.ts' }, status: 'success', callSeen: true, result: { content: 'export const rows = []', mediaType: 'text/typescript', isError: false, at: at(3) }, at: at(1) },
  { _tag: 'Reasoning', id: 'reasoning', text: 'Compare the observed selection first.', streaming: false, durationMs: 120, at: at(4) },
  { _tag: 'Text', id: 'answer', role: 'assistant', text: 'The projection keeps **visible rows** together.', attachments: [], streaming: false, at: at(5) },
  { _tag: 'Status', id: 'status', status: 'completed', detail: 'Finished', at: at(7) },
  { _tag: 'UnknownEvent', id: 'custom', eventType: 'custom', data: { kind: 'custom' }, at: at(8) },
  { _tag: 'Notice', id: 'truncated', kind: 'truncation', text: 'Older history unavailable in this transcript window', detail: 'retained · sequences 0–40 omitted', at: at(9) },
  prompt('p2', 'And verify the fix.', 10, { _tag: 'Pending' }),
  { _tag: 'ToolCall', id: 'run', callId: 'c2', name: 'bash', input: { command: 'pnpm test rows' }, status: 'running', callSeen: true, at: at(11) },
]

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

describe('kit transcript turn mapping', () => {
  it('splits the page into prompt-owned turns and keeps payloads by reference', () => {
    const turns = transcriptTurnsForItems(scenario, { agentName: 'Example Agent', firstTurnComplete: true })
    expect(turns.map(turn => turn.id)).toEqual(['p1', 'p2'])
    expect(turns[0]!.prompt).toBe(scenario[0])
    expect(turns[0]!.items.map(item => item.id)).toEqual(['read', 'reasoning', 'answer', 'status'])
    expect(turns[1]!.items.map(item => item.id)).toEqual(['run'])
  })

  it('omits unsupported event kinds and truncation notices structurally, not by text', () => {
    const turns = transcriptTurnsForItems(scenario, { firstTurnComplete: true })
    const items = turns.flatMap(turn => [turn.prompt, ...turn.items])
    expect(items.some(item => item._tag === 'UnknownEvent')).toBe(false)
    expect(items.some(item => item._tag === 'Notice' && item.kind === 'truncation')).toBe(false)
    const kept = transcriptTurnsForItems([
      { _tag: 'UnknownEvent', id: 'pin', eventType: 'credential_pin', data: {}, at: at(1) },
      { _tag: 'UnknownEvent', id: 'renamed', eventType: 'title_change', data: {}, at: at(2) },
      { _tag: 'Notice', id: 'kept', kind: 'redaction', text: 'Withheld 4 bytes', at: at(3) },
    ], { firstTurnComplete: true })
    expect(kept).toEqual([])
    const notices = transcriptTurnsForItems([
      prompt('p', 'Hello', 1),
      { _tag: 'Notice', id: 'redaction', kind: 'redaction', text: 'Withheld 4 bytes', at: at(2) },
    ], { firstTurnComplete: true })
    expect(notices[0]!.items.map(item => item._tag)).toEqual(['Notice'])
  })

  it('groups tool calls into the kit work log with host-owned classification', () => {
    const turns = transcriptTurnsForItems(scenario, { firstTurnComplete: true })
    expect(turns[0]!.work.calls.map(call => [call.id, call.kind])).toEqual([['read', 'read']])
    expect(turns[1]!.work.calls.map(call => [call.id, call.kind, call.status])).toEqual([['run', 'run', 'running']])
    expect(turns[0]!.work.running).toBe(false)
    expect(turns[0]!.work.durationMs).toBe(7000)
  })

  it('derives run lifecycle from the run status entries, not from row presence', () => {
    const failing = transcriptTurnsForItems([
      prompt('p', 'Try the deploy.', 0),
      { _tag: 'Status', id: 's1', status: 'running', at: at(1) },
      { _tag: 'Status', id: 's2', status: 'failed', detail: 'The command exited 1.', at: at(4) },
    ], { firstTurnComplete: true })
    expect(failing[0]!.work).toMatchObject({ running: false, failed: true, failureNote: 'The command exited 1.', interrupted: false })
    const cancelled = transcriptTurnsForItems([
      prompt('p', 'Stop here.', 0),
      { _tag: 'Status', id: 's1', status: 'cancelled', at: at(2) },
    ], { firstTurnComplete: true })
    expect(cancelled[0]!.work).toMatchObject({ interrupted: true, running: false })
    const live = transcriptTurnsForItems([
      prompt('p', 'Keep going.', 0),
      { _tag: 'Status', id: 's1', status: 'waiting', at: at(1) },
    ], { firstTurnComplete: true })
    expect(live[0]!.work.running).toBe(true)
    const unsignalled = transcriptTurnsForItems([scenario[0]!, scenario[8]!], { firstTurnComplete: true })
    expect(unsignalled[0]!.work.running).toBe(true)
  })

  it('passes optimistic send state through on its prompt', () => {
    const turns = transcriptTurnsForItems(scenario, { firstTurnComplete: true })
    expect(turns[1]!.prompt).toMatchObject({ id: 'p2', sendState: { _tag: 'Pending' } })
  })

  it('passes a failed send through 1:1 with its classified reason and detail', () => {
    const failed = prompt('p3', 'Retry me.', 12, { _tag: 'Failed', reason: 'stale-fence', detail: 'The conversation moved while sending.' })
    const turns = transcriptTurnsForItems([failed, { _tag: 'Text', id: 'answer', role: 'assistant', text: 'Later answer.', attachments: [], streaming: false, at: at(13) }], { firstTurnComplete: true })
    expect(turns[0]!.prompt.sendState).toEqual({ _tag: 'Failed', reason: 'stale-fence', detail: 'The conversation moved while sending.' })
  })

  it('captions assistant answers with the roster name and never invents one', () => {
    const named = transcriptTurnsForItems(scenario, { agentName: 'Example Agent', firstTurnComplete: true })
    expect(named[0]!.senderCaptions).toEqual({ answer: 'Example Agent' })
    const anonymous = transcriptTurnsForItems(scenario, { firstTurnComplete: true })
    expect(anonymous[0]!.senderCaptions).toBeUndefined()
  })

  it('keeps a truncated page’s first turn expanded', () => {
    const turns = transcriptTurnsForItems(scenario, { firstTurnComplete: false })
    expect(turns[0]!.work.foldable).toBe(false)
    // Later turns carry the kit default: foldable unless stated otherwise.
    expect(turns[1]!.work.foldable).toBeUndefined()
  })

  it('renders every kept item through the kit converter unchanged', () => {
    const items = transcriptTurnsForItems(scenario, { firstTurnComplete: true }).flatMap(turn => [turn.prompt, ...turn.items])
    for (const item of items) {
      const message = convertConversationItem(item)
      expect(message.id).toBe(item.id)
      expect(message.metadata?.custom?.item).toBe(item)
    }
  })
})

describe('conversation feed mapping', () => {
  it.each(['ungranted', 'unsupported', 'failed'] as const)('maps Unavailable(%s) to the kit availability state', reason => {
    const state = mapConversationFeed({ _tag: 'Unavailable', reason, detail: 'Actual diagnostic' }, {})
    expect(state).toMatchObject({ _tag: 'Unavailable', availability: { _tag: 'Unavailable' } })
    expect(JSON.stringify(state)).not.toContain('Actual diagnostic')
  })

  it.each([
    ['ungranted', undefined, 'ungranted', 'Conversation access not granted', 'Ask an administrator for read access to this conversation.'],
    ['unsupported', undefined, 'unsupported', 'Conversation not supported', 'This view cannot show this conversation yet.'],
    ['failed', 'not-found', 'not-found', 'Conversation not found', 'There is no conversation for this agent right now.'],
    ['failed', 'unavailable', 'failed', 'Conversation unavailable', 'The conversation could not be loaded.'],
    ['failed', undefined, 'failed', 'Conversation unavailable', 'The conversation could not be loaded.'],
  ] as const)('classifies Unavailable(%s, code %s) as %s with one fixed reason', (reason, code, classification, title, detail) => {
    const sentinel = 'raw-read-diagnostic-sentinel: the requested resource was not found'
    const state = mapConversationFeed({ _tag: 'Unavailable', reason, detail: sentinel, ...(code === undefined ? {} : { code }) }, {})
    expect(state).toEqual({ _tag: 'Unavailable', classification, code, availability: { _tag: 'Unavailable', reason: title, detail } })
    expect(JSON.stringify(state)).not.toContain('sentinel')
  })

  it('classifies not-found by the read error code, never by message text', () => {
    const state = mapConversationFeed({ _tag: 'Unavailable', reason: 'failed', detail: 'not-found' }, {})
    expect(state).toMatchObject({ classification: 'failed', availability: { reason: 'Conversation unavailable' } })
  })

  it.each(['not found', '<img src=x onerror=alert(1)>', 'Not-Found', '-leading', 'x'.repeat(41), ''])('omits a diagnostic code outside the allowlist (%s)', code => {
    const state = mapConversationFeed({ _tag: 'Unavailable', reason: 'failed', detail: 'diagnostic', code }, {})
    expect(state).toEqual({ _tag: 'Unavailable', classification: 'failed', code: undefined,
      availability: { _tag: 'Unavailable', reason: 'Conversation unavailable', detail: 'The conversation could not be loaded.' } })
  })

  it('keeps an allowlisted diagnostic code at the length limit', () => {
    const code = 'a'.repeat(40)
    expect(mapConversationFeed({ _tag: 'Unavailable', reason: 'failed', detail: 'diagnostic', code }, {})).toMatchObject({ code })
  })

  it('keeps waiting distinguishable from an observed page', () => {
    expect(mapConversationFeed({ _tag: 'Waiting' }, {})).toEqual({ _tag: 'Waiting' })
  })

  it('maps older history to the in-lane HasOlder boundary without internal wording', () => {
    const state = mapConversationFeed({ _tag: 'Observed', freshness: 'live', value: { items: scenario, hasOlder: true } }, { agentName: 'Example Agent' })
    expect(state).toMatchObject({ _tag: 'Observed', history: { _tag: 'HasOlder' } })
    expect(JSON.stringify(state)).not.toContain('sequences')
  })

  it('treats a truncation marker as the older-history boundary even without hasOlder', () => {
    const truncationOnly = scenario.filter(item => item.id !== 'custom')
    const state = mapConversationFeed({ _tag: 'Observed', freshness: 'live', value: { items: truncationOnly, hasOlder: false } }, {})
    expect(state).toMatchObject({ history: { _tag: 'HasOlder' } })
  })

  it('marks a filtered-empty page and routes its copy through the kit empty state', () => {
    const state = mapConversationFeed({ _tag: 'Observed', freshness: 'live', value: { items: [scenario[5]!], hasOlder: false, observation: { empty: false } } }, {})
    expect(state).toMatchObject({ _tag: 'Observed', turns: [], filteredEmpty: true, emptyState: { title: 'This page contains no displayable conversation entries.' } })
  })

  it('exposes runtime messages for exactly the prompts and kept items', () => {
    const state = mapConversationFeed({ _tag: 'Observed', freshness: 'live', value: { items: scenario, hasOlder: false } }, { agentName: 'Example Agent' })
    expect(state._tag === 'Observed' && state.items.map(item => item.id)).toEqual(['p1', 'read', 'reasoning', 'answer', 'status', 'p2', 'run'])
  })
})

describe('transcript sync mapping', () => {
  it.each(statuses)('passes the SDK verdict for $_tag through unchanged', status => {
    expect(transcriptSyncStatus({ status, observedAt: 123 }, { _tag: 'Waiting' }, 456)).toBe(status)
  })

  it('does not manufacture a sync verdict for a source that reports none', () => {
    expect(transcriptSyncStatus(undefined, { _tag: 'Waiting' }, 456)).toEqual({ _tag: 'Connecting', attempt: 1, since: 456 })
    expect(transcriptSyncStatus(undefined, { _tag: 'Observed', freshness: 'live', value: { items: [], hasOlder: false } }, 456)).toEqual({ _tag: 'Live', since: 456 })
    expect(transcriptSyncStatus(undefined, { _tag: 'Observed', freshness: 'stale', value: { items: [], hasOlder: false } }, 456)).toEqual({ _tag: 'Stale', reason: { _tag: 'Unknown' } })
  })

  it('stays read-only and exposes no mutation capabilities', async () => {
    const runtime = transcriptRuntimeOptions(scenario, true)
    expect(runtime.messages).toBe(scenario)
    expect(runtime.isRunning).toBe(true)
    expect(runtime.isDisabled).toBe(true)
    expect(runtime.onEdit).toBeUndefined()
    expect(runtime.onReload).toBeUndefined()
    expect(runtime.onCancel).toBeUndefined()
    await expect(runtime.onNew({
      role: 'user', content: [{ type: 'text', text: 'No send' }], createdAt: new Date(at(0)),
      metadata: { custom: {} }, parentId: null, sourceId: null, runConfig: undefined,
    })).rejects.toThrow('read-only')
    expect(transcriptRuntimeOptions()).toMatchObject({ messages: [], isRunning: false, isDisabled: true })
  })
})
