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

/**
 * Owner rule: unsupported native event kinds (omp `custom`, `credential_pin`, `title_change`, …)
 * and truncation notices never become transcript rows. The decision is structural — item tag and
 * notice kind — never a text match; the in-lane history boundary owns the truncation wording.
 */
type KitConversationItem = NonNullable<ConversationRuntimeOptions['messages']>[number]
type KitTextItem = Extract<KitConversationItem, { _tag: 'Text' }>

/** Send-state pass-through contract: the kit's accepted failure reasons must cover every app
 * reason. Today the kit takes free strings, so the assignment below is 1:1. When the kit closes
 * the enum (fractal's next head), this line is the compile error that flags any uncovered reason
 * instead of silently narrowing. */
const kitCoversAppFailureReasons: Extract<SendState, { _tag: 'Failed' }>['reason'] extends Extract<NonNullable<KitTextItem['sendState']>, { _tag: 'Failed' }>['reason'] ? true : never = true

const isPrompt = (item: KitConversationItem): item is KitTextItem & { readonly role: 'user' } =>
  item._tag === 'Text' && item.role === 'user'

const isOmittedItem = (item: ConversationItem): boolean =>
  item._tag === 'UnknownEvent' || (item._tag === 'Notice' && item.kind === 'truncation')

/** The host owns work-log classification: how a tool reads, runs or edits. */
const toolKindFor = (name: string): WorkKind => {
  const tool = name.toLowerCase()
  if (/(?:^|[/_.-])(?:bash|sh|shell|exec|execute|run|command)(?:$|[/_.-])/.test(tool)) return 'run'
  return /(?:edit|write|patch|apply)/.test(tool) ? 'edit' : 'read'
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

/**
 * Split a conversation page into prompt-owned turns for the kit composition. Kit A's
 * `TranscriptTurn` requires a prompt, so a truncated page's leading tail (items before the first
 * user text) stays omitted until the kit accepts prompt-less turns; the in-lane HasOlder boundary
 * states what is not loaded. Never synthesize a prompt.
 */
export const transcriptTurnsForItems = (
  items: readonly ConversationItem[],
  options: TranscriptTurnOptions,
): readonly TranscriptTurn[] => {
  const turns: TranscriptTurn[] = []
  let prompt: (KitTextItem & { readonly role: 'user' }) | undefined
  let turnItems: KitConversationItem[] = []
  const flush = () => {
    if (prompt === undefined) return
    const senderCaptions: Record<string, string | undefined> = {}
    for (const item of turnItems) {
      if (item._tag !== 'Text' || item.role !== 'assistant') continue
      const caption = item.sender?.label ?? options.agentName
      if (caption !== undefined) senderCaptions[item.id] = caption
    }
    turns.push({
      id: prompt.id,
      prompt,
      items: turnItems,
      work: workLogTurnFromItems([prompt, ...turnItems], {
        kindFor: toolKindFor,
        ...workFactsFor(prompt.at, turnItems),
        startedAt: prompt.at,
        ...(turns.length === 0 && !options.firstTurnComplete ? { completeHistory: false } : {}),
      }),
      ...(Object.keys(senderCaptions).length === 0 ? {} : { senderCaptions }),
    })
    turnItems = []
  }
  for (const item of items) {
    if (isOmittedItem(item)) continue
    if (isPrompt(item)) {
      flush()
      prompt = item
      continue
    }
    if (prompt !== undefined) turnItems.push(item)
  }
  flush()
  return turns
}

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
  ungranted: { reason: 'Conversation access not granted', detail: 'Ask an administrator for read access to this conversation.' },
  unsupported: { reason: 'Conversation not supported', detail: 'This view cannot show this conversation yet.' },
  'not-found': { reason: 'Conversation not found', detail: 'There is no conversation for this agent right now.' },
  failed: { reason: 'Conversation unavailable', detail: 'The conversation could not be loaded.' },
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
      // Either native signal states the boundary; neither leaks internal wording into rows.
      const hasOlder = feed.value.hasOlder
        || feed.value.items.some(item => item._tag === 'Notice' && item.kind === 'truncation')
      const turns = transcriptTurnsForItems(feed.value.items, { agentName: options.agentName, firstTurnComplete: !hasOlder })
      // Native page provenance decides emptiness: a non-empty page whose rows are all omitted
      // (or that arrived with none) is filtered, not an empty conversation.
      const filteredEmpty = feed.value.observation?.empty === false
        && (feed.value.items.length === 0 || feed.value.items.every(isOmittedItem))
      return {
        _tag: 'Observed',
        turns,
        items: turns.flatMap(turn => [turn.prompt, ...turn.items]),
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
