import type {
  ConversationRuntimeOptions,
  TranscriptAvailability,
  TranscriptEmptyState,
  TranscriptHistory,
  TranscriptTurn,
} from '@smalltalk/fractal-ui/assistant-ui'
import { workLogTurnFromItems, type WorkKind } from '@smalltalk/fractal-ui/assistant-ui/work-log'
import type { SyncStatus } from '@smalltalk/fractal-ui/assistant-ui/sync'
import type { ConversationItem, RunStatus, SendState } from '../conversation/model.ts'
import type { FeedSyncObservation } from '../data/feedSync.ts'
import type { ConversationPage, Feed } from '../data/source.ts'
import { fieldsOf } from '../conversation/semantics.ts'

/** The internal-event omit policy lives only here; unlisted kinds get a payload-free neutral row. */
type KitConversationItem = NonNullable<ConversationRuntimeOptions['messages']>[number]
type KitTextItem = Extract<KitConversationItem, { _tag: 'Text' }>

type KitSendFailureReason = Extract<NonNullable<KitTextItem['sendState']>, { _tag: 'Failed' }>['reason']

/** Exhaustive for app reasons; a future unclassified reason remains a generic failure. */
const sendFailureReasons: Readonly<Record<Extract<SendState, { _tag: 'Failed' }>['reason'], KitSendFailureReason>>
  & Readonly<Record<string, KitSendFailureReason | undefined>> = {
    rejected: { _tag: 'Rejected' },
    ungranted: { _tag: 'Ungranted' },
    invalid: { _tag: 'Invalid' },
    failed: { _tag: 'Failed' },
    'stale-fence': { _tag: 'StaleFence' },
    'snapshot-unavailable': { _tag: 'SnapshotUnavailable' },
  }

export const transcriptSendFailureReason = (reason: string): KitSendFailureReason => {
  const classified = Object.hasOwn(sendFailureReasons, reason) ? sendFailureReasons[reason] : undefined
  return classified ?? sendFailureReasons.failed
}

/** Only failed Text sends require an adapter allocation; every other source item stays by reference. */
const hasKitSendState = (item: ConversationItem): item is ConversationItem & KitConversationItem =>
  item._tag !== 'Text' || item.sendState?._tag !== 'Failed'

const isPrompt = (item: KitConversationItem): item is KitTextItem & { readonly role: 'user' } =>
  item._tag === 'Text' && item.role === 'user'

// Native source preserves raw omp records (`external_sessions.rs` push_unrecognized); the
// converter decodes their JSON envelope (`fromTimeline.ts` unrecognizedOmpItem). `custom` is
// a bucket, not a kind: its raw.customType distinguishes redundant tool-start bookkeeping.
// Model/thinking changes are native status metadata (external_sessions.rs push_unrecognized).
// model_usage is raw harness accounting, not an unsupported conversation event. Native `usage`
// entries already have the typed Usage projection and remain available to the kit.
const internalEventKinds: Record<string, true | undefined> = {
  credential_pin: true,
  title_change: true,
  model_change: true,
  thinking_level_change: true,
  model_usage: true,
  'custom/model_usage': true,
  'message-role/developer': true,
  'message-role/system-reminder': true,
  'custom/tool_execution_start': true,
}

const isOmittedItem = (item: ConversationItem): boolean => {
  if (item._tag !== 'UnknownEvent') return false
  const raw = fieldsOf(fieldsOf(item.data)['raw'])
  const customType = raw['customType']
  const kind = item.eventType === 'custom' && typeof customType === 'string'
    ? `custom/${customType}` : item.eventType
  return Object.hasOwn(internalEventKinds, kind)
}

/** The host owns work-log classification: how a tool reads, runs or edits. */
const toolKindFor = (name: string): WorkKind => {
  const tool = name.toLowerCase()
  if (/(?:^|[/_.-])(?:bash|sh|shell|exec|execute|run|command|eval|task|infra)(?:$|[/_.-])/.test(tool)) return 'run'
  return /(?:edit|write|patch|apply)/.test(tool) ? 'edit' : 'read'
}

/** Human intent belongs in the header; raw paths and commands remain on the source tool item. */
const toolSummary = (item: Extract<KitConversationItem, { _tag: 'ToolCall' }>): string => {
  const input = typeof item.input === 'object' && item.input !== null ? item.input : {}
  for (const key of ['i', 'title', 'description']) {
    const value = key in input ? Reflect.get(input, key) : undefined
    if (typeof value === 'string' && value.trim().length > 0 && !/(?:[a-z]+:\/\/|\/[\w.-]+\/)/i.test(value))
      return value.trim().replace(/\s+/g, ' ').slice(0, 100)
  }
  return toolKindFor(item.name) === 'run' ? 'Running a command'
    : toolKindFor(item.name) === 'edit' ? 'Updating a file' : 'Reading information'
}

const activeRunStatuses: Record<RunStatus, boolean> = { queued: true, running: true, waiting: true, completed: false, failed: false, cancelled: false }

const lastStatusOf = (items: readonly KitConversationItem[]) => {
  for (let index = items.length - 1; index >= 0; index -= 1) {
    const item = items[index]
    if (item?._tag === 'Status') return item
  }
  return undefined
}

const elapsedBetween = (start: string, end: string): number | undefined => {
  const from = Date.parse(start)
  const to = Date.parse(end)
  return Number.isFinite(from) && Number.isFinite(to) && to >= from ? to - from : undefined
}

/** Run lifecycle comes from the run's own status entries; an unanswered call stays in flight. */
const workFactsFor = (promptAt: string, items: readonly KitConversationItem[]) => {
  const status = lastStatusOf(items)
  const running = status !== undefined
    ? activeRunStatuses[status.status]
    : items.some(item => item._tag === 'ToolCall' && item.status === 'running')
  const settledAt = status !== undefined && !activeRunStatuses[status.status] ? status.at : undefined
  return {
    running,
    failed: status?.status === 'failed',
    interrupted: status?.status === 'cancelled',
    ...(status?.status === 'failed' && status.detail !== undefined ? { failureNote: status.detail } : {}),
    ...(settledAt !== undefined ? { durationMs: elapsedBetween(promptAt, settledAt) } : {}),
  }
}

export interface TranscriptTurnOptions {
  /** Roster display name for assistant captions; never invented when absent. */
  readonly agentName?: string
  /** A page with older history may have cut its first turn's earlier entries. */
  readonly firstTurnComplete: boolean
}

/** Prepared turns preserve the leading native tail without synthesizing a human prompt. */
export type PreparedTranscriptTurn = Omit<TranscriptTurn, 'prompt'> & {
  readonly prompt?: KitTextItem & { readonly role: 'user' }
}

export const prepareTranscriptTurns = (
  items: readonly ConversationItem[],
  options: TranscriptTurnOptions,
): readonly PreparedTranscriptTurn[] => {
  const turns: PreparedTranscriptTurn[] = []
  let prompt: (KitTextItem & { readonly role: 'user' }) | undefined
  let turnItems: KitConversationItem[] = []
  const flush = () => {
    const first = prompt ?? turnItems[0]
    if (first === undefined) return
    const senderCaptions: Record<string, string | undefined> = {}
    for (const item of turnItems) {
      if (item._tag !== 'Text' && item._tag !== 'Message') continue
      const label = item.sender?.label ?? (item._tag === 'Message' ? item.from : item.role === 'assistant' ? options.agentName : undefined)
      const caption = label !== undefined && /^(?:proc|agent):\/\//i.test(label)
        ? item._tag === 'Text' && item.role === 'assistant' ? options.agentName ?? 'Assistant' : /^proc:/i.test(label) ? 'Process' : 'Agent'
        : label
      if (caption !== undefined) senderCaptions[item.id] = caption
    }
    const work = workLogTurnFromItems(prompt === undefined ? turnItems : [prompt, ...turnItems], {
      kindFor: toolKindFor,
      ...workFactsFor(first.at ?? '', turnItems),
      ...(first.at === undefined ? {} : { startedAt: first.at }),
      ...(turns.length === 0 && !options.firstTurnComplete ? { completeHistory: false } : {}),
    })
    const tools = new Map(turnItems.flatMap(item => item._tag === 'ToolCall' ? [[item.id, item] as const] : []))
    turns.push({
      id: first.id,
      ...(prompt === undefined ? {} : { prompt }),
      items: turnItems,
      work: { ...work, calls: work.calls.map(call => {
        const item = tools.get(call.id)
        return { ...call, title: item === undefined ? 'Working' : toolSummary(item), argsSummary: undefined }
      }) },
      ...(Object.keys(senderCaptions).length === 0 ? {} : { senderCaptions }),
    })
    turnItems = []
  }
  for (const sourceItem of items) {
    if (isOmittedItem(sourceItem)) continue
    // The runtime must not receive unknown protocol payloads either: its converter and fallback
    // rows render the same short notice, never a raw JSON envelope or producer-specific kind.
    const item: KitConversationItem = sourceItem._tag === 'UnknownEvent'
      ? { _tag: 'Notice', id: sourceItem.id, kind: 'event', text: 'An event this view cannot show yet.',
        ...(sourceItem.at === undefined ? {} : { at: sourceItem.at }) }
      : hasKitSendState(sourceItem) ? sourceItem
        : { ...sourceItem, sendState: sourceItem.sendState?._tag === 'Failed'
          ? { ...sourceItem.sendState, reason: transcriptSendFailureReason(sourceItem.sendState.reason) }
          : sourceItem.sendState }
    if (isPrompt(item)) {
      flush()
      prompt = item
      continue
    }
    turnItems.push(item)
  }
  flush()
  return turns
}

/** Prompt-less native tails are supported by the kit without a synthetic user bubble. */

export const transcriptTurnsForItems = (
  items: readonly ConversationItem[],
  options: TranscriptTurnOptions,
): readonly TranscriptTurn[] => prepareTranscriptTurns(items, options)

export type ConversationTranscriptState =
  | { readonly _tag: 'Waiting' }
  | {
      readonly _tag: 'Unavailable'
      readonly classification: UnavailableClassification
      /** The read error's machine code: data-wf-* diagnostics only. */
      readonly code: string | undefined
      readonly availability: TranscriptAvailability
    }
  | {
      readonly _tag: 'Observed'
      readonly turns: readonly TranscriptTurn[]
      /** Runtime messages: exactly the prompts and kept items the transcript may render. */
      readonly items: readonly KitConversationItem[]
      readonly history: TranscriptHistory
      readonly filteredEmpty: boolean
      readonly emptyState: TranscriptEmptyState | undefined
      readonly isRunning: boolean
    }

/** Why the pane cannot show a conversation; `not-found` comes from the read error's code, never its message. */
export type UnavailableClassification = 'ungranted' | 'unsupported' | 'not-found' | 'failed'

/** Fixed copy only: feed.detail carries source diagnostics and never reaches rendered or accessible text. */
const unavailableCopy: Readonly<Record<UnavailableClassification, { readonly reason: string; readonly detail: string }>> = {
  ungranted: { reason: 'Conversation access not granted', detail: 'Ask the gateway owner to grant access to this surface. Your saved work is unchanged.' },
  unsupported: { reason: 'Conversation not supported', detail: 'This gateway does not support this surface. Choose another surface; your saved work is unchanged.' },
  'not-found': { reason: 'Conversation not found', detail: 'There is no conversation for this agent right now. Ask the gateway owner to grant access to this surface. Your saved work is unchanged.' },
  failed: { reason: 'Conversation unavailable', detail: 'The gateway could not load this surface. Reload to reconnect; your saved work is unchanged.' },
}

/** Server codes are unrestricted strings: only a short lowercase slug may reach a data-wf attribute. */
const diagnosticCode = /^[a-z][a-z0-9-]{0,39}$/

export const mapConversationFeed = (
  feed: Feed<ConversationPage>,
  options: { readonly agentName?: string },
): ConversationTranscriptState => {
  switch (feed._tag) {
    case 'Waiting':
      return { _tag: 'Waiting' }
    case 'Unavailable': {
      const classification: UnavailableClassification = feed.reason === 'failed' && feed.code === 'not-found' ? 'not-found' : feed.reason
      const code = feed.code !== undefined && diagnosticCode.test(feed.code) ? feed.code : undefined
      return { _tag: 'Unavailable', classification, code, availability: { _tag: 'Unavailable', ...unavailableCopy[classification] } }
    }
    case 'Observed': {
      const hasOlder = feed.value.hasOlder
      const turns = transcriptTurnsForItems(feed.value.items, { agentName: options.agentName, firstTurnComplete: !hasOlder })
      // Native page provenance decides emptiness: a non-empty page whose rows are all omitted
      // (or that arrived with none) is filtered, not an empty conversation.
      const filteredEmpty = feed.value.observation?.empty === false
        && (feed.value.items.length === 0 || feed.value.items.every(isOmittedItem))
      return {
        _tag: 'Observed',
        turns,
        items: turns.flatMap(turn => turn.prompt === undefined ? turn.items : [turn.prompt, ...turn.items]),
        history: hasOlder ? { _tag: 'HasOlder' } : { _tag: 'Complete' },
        filteredEmpty,
        emptyState: filteredEmpty ? { title: 'This page contains no displayable conversation entries.' } : undefined,
        isRunning: turns.some(turn => turn.work.running),
      }
    }
  }
}


/**
 * The SDK status stream owns every Live verdict. A source that reports no sync stream (fixtures)
 * declares freshness on its feed instead; retained rows never manufacture Live.
 */
export const transcriptSyncStatus = (
  observation: FeedSyncObservation | undefined,
  feed: Feed<ConversationPage>,
  now: number,
): SyncStatus =>
  observation !== undefined ? observation.status
    : feed._tag === 'Observed' && feed.freshness === 'live' ? { _tag: 'Live', since: now }
      : feed._tag === 'Observed' ? { _tag: 'Stale', reason: { _tag: 'Unknown' } }
        : { _tag: 'Connecting', attempt: 1, since: now }

export const transcriptObservedAt = (observation: FeedSyncObservation | undefined, now: number): number =>
  observation?.observedAt ?? now

/** Read-only transcript capabilities: no send/edit/retry transport is claimed by this layer. */
export const transcriptRuntimeOptions = (
  items: readonly KitConversationItem[] = [],
  isRunning = false,
): ConversationRuntimeOptions => ({
  messages: items,
  isRunning,
  isDisabled: true,
  onNew: async () => { throw new Error('This conversation view is read-only') },
})

/** Agent text is untrusted: only plain web URLs may leave the app, never javascript:, data: or blob: targets. */
export const openableImageUrl = (src: string): string | undefined => {
  try {
    const url = new URL(src)
    return url.protocol === 'https:' || url.protocol === 'http:' ? url.href : undefined
  } catch {
    return undefined
  }
}
