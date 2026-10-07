import type { ConversationItem, Sender } from './embrace-data/model.ts'
import { normalizePublicConversation } from './embrace-data/normalize-public-world.ts'
import { generatePublicConversations } from './embrace-data/public-world.ts'

export type { ConversationItem, Sender, ToolCallItem } from './embrace-data/model.ts'
export type EmbraceState = 'idle' | 'streaming' | 'tool-running' | 'error' | 'empty' | 'read-only'

/** Synthetic fixture world seed-138 conversations, including Worker 2's genuinely unresolved call. */
export const publicSessionItems: readonly ConversationItem[] = generatePublicConversations()
  .flatMap((conversation) => normalizePublicConversation(conversation))
  .sort((left, right) => (left.at ?? '').localeCompare(right.at ?? '') || left.id.localeCompare(right.id))

const baseTime = Date.parse('2032-01-18T23:59:06.000Z')
const at = (offset: number): string => new Date(baseTime + offset * 1000).toISOString()
const worker: Sender = { kind: 'agent', label: 'Worker 1' }
const peer: Sender = { kind: 'st-agent', label: 'Worker 3', via: 'delivery' }
const subagent: Sender = { kind: 'subagent', label: 'Sample reviewer', via: 'Worker 1' }
const harness: Sender = { kind: 'harness', label: 'Synthetic harness' }
const system: Sender = { kind: 'system', label: 'System' }
const operator: Sender = { kind: 'human', label: 'Operator' }

export const syntheticDiff = [
  '--- a/sample/rows.ts', '+++ b/sample/rows.ts', '@@ -1 +1 @@',
  '-export const rowCount = 3', '+export const rowCount = 4',
].join('\n')
export const syntheticMarkdown = '# Synthetic sample\n\nFour rows are ready for review.\n\n- All input is invented.\n- Each row has a stable identifier.\n\n```ts\nexport const rowCount = 4\n```'

/**
 * Semantic coverage derived from the public world's sample-assembly task. The public generator
 * emits only message/content/tool-call/result; these invented continuations exercise the real
 * model's remaining tags without importing the app's world, captures or another role model.
 * model.ts has nine tags.
 */
export const semanticCoverageItems: readonly ConversationItem[] = [
  {
    _tag: 'Status', id: 'sample/status-queued', status: 'queued', detail: 'Sample review queued', at: at(0),
  },
  {
    _tag: 'Status', id: 'sample/status-running', status: 'running', detail: 'Assembling four rows', at: at(1),
  },
  {
    _tag: 'Reasoning', id: 'sample/reasoning', text: 'Compare the requested row count with the input, then ask a second worker to review the change.',
    durationMs: 1250, streaming: false, at: at(2), sender: worker,
  },
  {
    _tag: 'Text', id: 'sample/review-request', role: 'assistant',
    text: 'The input has **four rows**. I will update the count and ask the sample reviewer to inspect the diff.',
    attachments: [], streaming: false, at: at(3), sender: worker,
  },
  {
    _tag: 'ToolCall', id: 'sample/edit', callId: 'sample-call-edit', name: 'edit',
    input: { path: 'sample/rows.ts', oldString: 'export const rowCount = 3', newString: 'export const rowCount = 4' },
    status: 'success', callSeen: true, at: at(4), sender: worker,
    result: { content: syntheticDiff, mediaType: 'text/x-diff', isError: false, at: at(5) },
  },
  {
    _tag: 'ToolCall', id: 'sample/write', callId: 'sample-call-write', name: 'write',
    input: { path: 'sample/README.md', content: syntheticMarkdown },
    status: 'success', callSeen: true, at: at(6), sender: worker,
    result: { content: syntheticMarkdown, mediaType: 'text/markdown', isError: false, at: at(7) },
  },
  {
    _tag: 'Message', id: 'sample/peer-envelope', messageId: 'message/synthetic-138/review',
    from: 'agent/synthetic-138/3', to: 'agent/synthetic-138/1', title: 'Review the sample count',
    replyTo: 'message/synthetic-138/1', at: at(8), sender: peer,
  },
  {
    // Deliveries can arrive in user-role content without becoming human prompts.
    _tag: 'Text', id: 'sample/peer-delivery', role: 'user',
    text: 'Worker 3: I inspected the generated input. The count should remain four.',
    attachments: [], streaming: false, at: at(9), sender: peer,
  },
  {
    _tag: 'Event', id: 'sample/subagent-result', kind: 'subagent-result', title: 'Sample review complete',
    text: 'The diff and the markdown summary agree on four rows.', sender: subagent,
    data: { rows: 4, reviewedPaths: ['sample/rows.ts', 'sample/README.md'] }, at: at(10),
  },
  {
    _tag: 'Text', id: 'sample/subagent-text', role: 'assistant',
    text: 'No additional edits are needed. The sample is ready for the operator.',
    attachments: [], streaming: false, at: at(11), sender: subagent,
  },
  {
    _tag: 'ToolCall', id: 'sample/interrupted-question', callId: 'sample-call-question', name: 'ask_user',
    input: { question: 'Choose a synthetic sample size.', options: ['Four rows', 'Eight rows'] },
    status: 'interrupted', callSeen: true, at: at(12), sender: worker,
  },
  {
    _tag: 'ToolCall', id: 'sample/tool-error', callId: 'sample-call-inspect', name: 'bash',
    input: { command: 'inspect sample' }, status: 'error', callSeen: true, at: at(13), sender: worker,
    result: { content: 'Synthetic inspection exited with code 1: input unavailable.', mediaType: 'text/plain', isError: true, at: at(14) },
  },
  {
    _tag: 'ToolCall', id: 'sample/orphan-result', callId: 'sample-call-orphan', name: 'tool result',
    input: undefined, status: 'success', callSeen: false, at: at(15), sender: harness,
    result: { content: { rows: 4 }, mediaType: 'application/json', isError: false, at: at(15) },
  },
  {
    _tag: 'Event', id: 'sample/resource-observed', kind: 'resource-observed', title: 'Sample input observed',
    text: 'The synthetic input now contains four rows.', sender: harness,
    data: { path: '/srv/work/sample/input.json', rows: 4 }, at: at(16),
  },
  {
    _tag: 'Event', id: 'sample/harness-message', kind: 'harness-message', title: 'History checkpoint',
    text: 'The synthetic session checkpoint is available.', sender: harness,
    data: { checkpoint: 'sample-checkpoint-1' }, at: at(17),
  },
  {
    _tag: 'Notice', id: 'sample/redaction', kind: 'redaction', text: 'Withheld 32 synthetic bytes',
    detail: 'Intentional redaction fixture; no secret data exists.', at: at(18), sender: system,
  },
  {
    _tag: 'Notice', id: 'sample/truncation', kind: 'truncation', text: 'Older sample history is outside this window',
    detail: 'Synthetic sequences 1–3 omitted from the source window.', at: at(19), sender: harness,
  },
  {
    _tag: 'Notice', id: 'sample/event-notice', kind: 'event', text: 'The sample checkpoint was stored', at: at(20), sender: harness,
  },
  {
    _tag: 'Notice', id: 'sample/unknown-notice', kind: 'unknown', text: 'A future sample entry has no semantic renderer', at: at(21), sender: system,
  },
  {
    _tag: 'UnknownEvent', id: 'sample/future-event', eventType: 'sample.progress.v2',
    data: { phase: 'review', rows: 4, extension: { supported: false } }, at: at(22), sender: harness,
  },
  {
    // Optional timestamps and absent provenance are legitimate source-model cases.
    _tag: 'UnknownEvent', id: 'sample/undated-event', eventType: 'sample.undated', data: { observed: false },
  },
  {
    _tag: 'Text', id: 'sample/system-text', role: 'system', text: 'Synthetic session policy: generated input only.',
    attachments: [], streaming: false, at: at(23), sender: system,
  },
  {
    _tag: 'Text', id: 'sample/attachment', role: 'user', text: 'Keep the generated diagram with the sample.',
    attachments: [{ id: 'attachment/synthetic-138/diagram', mediaType: 'image/svg+xml', name: 'sample-diagram.svg' }],
    streaming: false, at: at(24), sender: operator,
  },
  {
    _tag: 'Usage', id: 'sample/response-usage', semantics: 'response', model: 'synthetic-model',
    inputTokens: 2200, outputTokens: 460, cachedTokens: 1600, cost: 0.012, currency: 'USD', at: at(25),
  },
  {
    _tag: 'Usage', id: 'sample/context-usage', semantics: 'context_occupancy', model: 'synthetic-model',
    contextUsedPercent: 34, at: at(26),
  },
  {
    _tag: 'Usage', id: 'sample/session-usage', semantics: 'session_cumulative',
    inputTokens: 6800, outputTokens: 1200, cachedTokens: 4000, cost: 0.032, currency: 'USD', at: at(27),
  },
  {
    _tag: 'Status', id: 'sample/status-waiting', status: 'waiting', detail: 'Operator review pending', at: at(28),
  },
  {
    _tag: 'Text', id: 'sample/summary', role: 'assistant', text: 'The synthetic sample is ready. **Four rows**, one reviewed diff, and a markdown summary are available above.',
    attachments: [], streaming: false, at: at(29), sender: worker,
  },
  {
    _tag: 'Status', id: 'sample/status-completed', status: 'completed', detail: 'Sample assembly complete', at: at(30),
  },
]

// The archival/idle view must not leave the public world's unresolved call spinning forever.
const archivedPublicItems = publicSessionItems.map((item): ConversationItem => {
  if (item._tag === 'ToolCall' && item.status === 'running') return { ...item, status: 'interrupted' }
  if (item._tag === 'Text' && item.streaming) return { ...item, streaming: false }
  return item
})
const idleItems: readonly ConversationItem[] = [...archivedPublicItems, ...semanticCoverageItems]
const streamingItems: readonly ConversationItem[] = [
  ...idleItems,
  { _tag: 'Status', id: 'sample/stream-status', status: 'running', detail: 'Summarizing review', at: at(31) },
  { _tag: 'Reasoning', id: 'sample/stream-reasoning', text: 'I will explain the reviewed change', streaming: true, at: at(32), sender: worker },
  { _tag: 'Text', id: 'sample/stream-text', role: 'assistant', text: 'The review confirms four rows. I am preparing the final', attachments: [], streaming: true, at: at(33), sender: worker },
]
const toolRunningItems: readonly ConversationItem[] = [
  ...idleItems,
  { _tag: 'Status', id: 'sample/tool-status', status: 'running', detail: 'Checking the next synthetic sample', at: at(31) },
  { _tag: 'ToolCall', id: 'sample/running-read', callId: 'sample-call-running-read', name: 'read', input: { path: '/srv/work/sample/input.json' }, status: 'running', callSeen: true, at: at(32), sender: worker },
  { _tag: 'ToolCall', id: 'sample/running-question', callId: 'sample-call-running-question', name: 'ask_user', input: { question: 'Choose a synthetic sample size.', options: ['Four rows', 'Eight rows'] }, status: 'running', callSeen: true, at: at(33), sender: worker },
]
const errorItems: readonly ConversationItem[] = [
  ...idleItems,
  { _tag: 'Status', id: 'sample/failed-status', status: 'failed', detail: 'Synthetic input unavailable', at: at(31) },
  { _tag: 'Notice', id: 'sample/error-notice', kind: 'error', text: 'The next synthetic sample could not be inspected.', detail: 'Generated failure: input unavailable.', retryable: true, at: at(32), sender: harness },
]
const readOnlyItems: readonly ConversationItem[] = [
  ...idleItems,
  { _tag: 'Status', id: 'sample/cancelled-status', status: 'cancelled', detail: 'Archived sample session', at: at(31) },
  { _tag: 'Notice', id: 'sample/archive-notice', kind: 'event', text: 'This generated session is read-only.', retryable: false, at: at(32), sender: harness },
]
const fixtures: Readonly<Record<EmbraceState, readonly ConversationItem[]>> = {
  idle: idleItems,
  streaming: streamingItems,
  'tool-running': toolRunningItems,
  error: errorItems,
  empty: [],
  'read-only': readOnlyItems,
}

export const fixtureForState = (state: EmbraceState): readonly ConversationItem[] => fixtures[state]

/**
 * 2,400 actual heterogeneous ConversationItems, not preconverted assistant-ui messages.
 * The app's long-session generator imports generated st3 codecs + the SDK + semantic parsers.
 * Rather than copying that dependency graph into the workshop, deterministically repeat the
 * normalized synthetic-world conversations and their invented semantic continuation. Each repeated
 * item, tool call, message envelope, attachment and timestamp has stable session-local identity.
 * Sender provenance and payloads remain unchanged, so grouping and tool previews are still real.
 */
export const longSessionItems: readonly ConversationItem[] = Array.from({ length: 2400 }, (_, index): ConversationItem => {
  const original = idleItems[index % idleItems.length]!
  const cycle = Math.floor(index / idleItems.length)
  const suffix = `:long-${cycle}`
  const timestamp = new Date(baseTime + index * 700).toISOString()
  const item = { ...original, id: `${original.id}${suffix}`, at: timestamp }
  switch (item._tag) {
    case 'ToolCall':
      return {
        ...item, callId: `${item.callId}${suffix}`,
        ...(item.result === undefined ? {} : {
          result: { ...item.result, at: new Date(baseTime + index * 700 + 350).toISOString() },
        }),
      }
    case 'Message':
      return {
        ...item, messageId: `${item.messageId}${suffix}`,
        ...(item.replyTo === undefined ? {} : { replyTo: `${item.replyTo}${suffix}` }),
      }
    case 'Text':
      return { ...item, attachments: item.attachments.map((attachment) => ({ ...attachment, id: `${attachment.id}${suffix}` })) }
    case 'Reasoning':
    case 'Status':
    case 'Usage':
    case 'Notice':
    case 'Event':
    case 'UnknownEvent':
      return item
    default: {
      const unhandled: never = item
      throw new Error(`Unrecognized long-session item: ${unhandled}`)
    }
  }
})
