// Project decoded st3 conversation chunks into the conversation view model.
// LiveTimeline preserves unchanged item identities and re-projects only the touched suffix.

import { TimelineEntryId, Timestamp, type TimelineEntry } from '@smalltalk/st3-client/schema'
import type { ConversationChunk, UnrecognizedEntry } from '@st3/sdk/effect'
import { DateTime, Option, Schema } from 'effect'

import type { ConversationItem, ToolCallItem, ToolResult } from './model.ts'
import { contentEvent, proseItem, withoutShownDeliveries } from './semantics.ts'

/**
 * Fixture-only D08 proposal, not part of the st3 wire contract.
 * Live SDK chunks never decode reasoning as this proposed entry.
 */
export const ProposedReasoningEntry = Schema.Struct({
  id: TimelineEntryId,
  sequence: Schema.Int.check(Schema.isGreaterThanOrEqualTo(0)),
  revision: Schema.Int.check(Schema.isGreaterThanOrEqualTo(1)),
  final: Schema.Boolean,
  role: Schema.Literals(['system', 'user', 'assistant', 'tool']),
  timestamp: Timestamp,
  type: Schema.Literal('reasoning'),
  body: Schema.Struct({ text: Schema.String, duration_ms: Schema.optionalKey(Schema.Finite) }),
})
export type ProposedReasoningEntry = typeof ProposedReasoningEntry.Type

/** Story chunks extend decoded SDK entries only with the fixture-only reasoning proposal. */
export type FixtureConversationChunk = Omit<ConversationChunk, 'entries'> & {
  readonly entries: ReadonlyArray<ConversationChunk['entries'][number] | ProposedReasoningEntry>
}

type Entry = TimelineEntry | ProposedReasoningEntry | UnrecognizedEntry
type ToolResultEntry = Extract<Entry, { type: 'tool_result' }>

const entryTimestamp = (entry: Exclude<Entry, UnrecognizedEntry>): string =>
  DateTime.formatIso(entry.timestamp)

/** No status yet, or queued/running/waiting: an unanswered call is still in flight. */
const isActive = (lastStatus: Entry | undefined) =>
  lastStatus?.type !== 'status' ||
  !(
    typeof lastStatus.body.status === 'string' &&
    ['completed', 'failed', 'cancelled'].includes(lastStatus.body.status)
  )

const resultOf = (entry: ToolResultEntry): ToolResult => ({
  content: entry.body.content,
  mediaType: entry.body.media_type,
  isError: entry.body.status === 'error',
  at: entryTimestamp(entry),
})

/** A call with its result joined (CAG.CLI.WEB.CNV-R04), or an orphan result rendered on its own. */
const joinResult = ({
  call,
  entry,
}: {
  readonly call: ToolCallItem | undefined
  readonly entry: ToolResultEntry
}): ToolCallItem =>
  call !== undefined
    ? { ...call, status: entry.body.status, result: resultOf(entry) }
    : {
        _tag: 'ToolCall',
        id: entry.id,
        callId: entry.body.call_id,
        name: 'tool result',
        input: undefined,
        status: entry.body.status,
        result: resultOf(entry),
        callSeen: false,
        at: entryTimestamp(entry),
      }

/**
 * st's native-transcript reader (`external_sessions.rs` `push_message`) emits a `message` entry
 * carrying only the harness's own message id before each transcript message's parts. It is a turn
 * header, not a Small Talk delivery (those always name `from`/`to`), so it renders nothing; the
 * parts after it carry the content.
 */
const isTurnHeader = (entry: Entry) =>
  entry.type === 'message' && entry.body.from === undefined && entry.body.to === undefined

/**
 * Ids st mints for a mailbox message's own content (`client_v0.rs`): the native projection
 * pairs `<leaf>/<digest16>-message` with `<leaf>/<digest16>-content`; the stored fallback mints
 * `<leaf>/<digest24>`. Harness turns carry `native-<n>` or their driver's own ids. Provenance is
 * read from the entry itself, so it holds when the header is paged out of the window.
 */
const MAILBOX_CONTENT_ID = /^timeline-entry\/[^/]+\/(?:[0-9a-f]{16}-content|[0-9a-f]{24})$/

/**
 * Join only st-minted mailbox pairs, never adjacent harness prose. Native pairs share a digest;
 * stored pairs have separate digests but share their claim's session, timestamp and sequence slot.
 */
const mailboxPairKey = (entry: Entry): string | undefined => {
  if (entry.type !== 'message' && entry.type !== 'content') return undefined
  const native = /^(timeline-entry\/[^/]+\/[0-9a-f]{16})-(message|content)$/.exec(entry.id)
  if (native !== null) return native[2] === entry.type ? native[1] : undefined
  const stored = /^(timeline-entry\/[^/]+)\/[0-9a-f]{24}$/.exec(entry.id)
  return stored === null ? undefined
    : `${stored[1]}|${entryTimestamp(entry)}|${entry.sequence - (entry.type === 'content' ? 1 : 0)}`
}
const decodeUnknownJson = Schema.decodeUnknownSync(Schema.fromJsonString(Schema.Unknown))

/** st3 explicitly marks unsupported omp entries; ordinary prose/JSON is never guessed at. */
const unrecognizedOmpItem = (
  entry: Extract<Entry, { type: 'content' }>,
): ConversationItem | undefined => {
  if (entry.role !== 'system' || entry.body.media_type !== 'text/plain') return undefined
  const text = entry.body.text ?? ''
  const marker =
    /^\[unrecognized omp (?:entry|content block)(?: `([^`]+)`| without a type)\]\n/.exec(text)
  if (marker === null) return undefined
  let data: unknown
  try {
    data = decodeUnknownJson(text.slice(marker[0].length))
  } catch {
    data = { rawText: text }
  }
  return {
    _tag: 'UnknownEvent',
    id: entry.id,
    at: entryTimestamp(entry),
    eventType: marker[1] ?? 'untyped omp entry',
    data,
  }
}

/** Projects one entry that is not a tool result (results join their call instead). */
const itemOf = ({
  entry,
  sessionActive,
}: {
  readonly entry: Exclude<Entry, ToolResultEntry>
  readonly sessionActive: boolean
}): ConversationItem => {
  switch (entry.type) {
    case 'content': {
      const internal = unrecognizedOmpItem(entry)
      if (internal !== undefined) return internal
      const event = contentEvent({
        id: entry.id,
        text: entry.body.text ?? '',
        mediaType: entry.body.media_type,
        at: entryTimestamp(entry),
      })
      if (event !== undefined) return event
      return proseItem({
        _tag: 'Text',
        id: entry.id,
        role: entry.role === 'tool' ? 'system' : entry.role,
        text: entry.body.text ?? '',
        attachments:
          entry.body.attachment_id !== undefined
            ? [{ id: entry.body.attachment_id, mediaType: entry.body.media_type }]
            : [],
        streaming: !entry.final,
        at: entryTimestamp(entry),
      })
    }
    case 'message':
      return {
        _tag: 'Message',
        id: entry.id,
        messageId: entry.body.message_id,
        ...(entry.body.from !== undefined ? { from: entry.body.from } : {}),
        ...(entry.body.to !== undefined ? { to: entry.body.to } : {}),
        ...(entry.body.title !== undefined ? { title: entry.body.title } : {}),
        ...(Option.isSome(entry.body.reply_to) ? { replyTo: entry.body.reply_to.value } : {}),
        at: entryTimestamp(entry),
      }
    case 'reasoning':
      return {
        _tag: 'Reasoning',
        id: entry.id,
        text: entry.body.text,
        ...(entry.body.duration_ms !== undefined ? { durationMs: entry.body.duration_ms } : {}),
        streaming: !entry.final,
        at: entryTimestamp(entry),
      }
    case 'tool_call':
      return {
        _tag: 'ToolCall',
        id: entry.id,
        callId: entry.body.call_id,
        name: entry.body.name,
        input: entry.body.arguments,
        status: sessionActive ? 'running' : 'interrupted',
        callSeen: true,
        at: entryTimestamp(entry),
      }
    case 'status':
      if (typeof entry.body.status !== 'string') {
        return {
          _tag: 'UnknownEvent',
          id: entry.id,
          eventType: `status: ${entry.body.status.raw}`,
          data: entry.body,
          at: entryTimestamp(entry),
        }
      }
      return {
        _tag: 'Status',
        id: entry.id,
        status: entry.body.status,
        ...(entry.body.detail !== undefined ? { detail: entry.body.detail } : {}),
        at: entryTimestamp(entry),
      }
    case 'usage':
      return {
        _tag: 'Usage',
        id: entry.id,
        semantics: entry.body.semantics,
        ...(entry.body.model !== undefined ? { model: entry.body.model } : {}),
        ...(entry.body.input_tokens !== undefined ? { inputTokens: entry.body.input_tokens } : {}),
        ...(entry.body.output_tokens !== undefined
          ? { outputTokens: entry.body.output_tokens }
          : {}),
        ...(entry.body.cached_tokens !== undefined
          ? { cachedTokens: entry.body.cached_tokens }
          : {}),
        ...(entry.body.cost !== undefined ? { cost: entry.body.cost } : {}),
        ...(entry.body.currency !== undefined ? { currency: entry.body.currency } : {}),
        ...(entry.body.context_used_percent !== undefined
          ? { contextUsedPercent: entry.body.context_used_percent }
          : {}),
        at: entryTimestamp(entry),
      }
    case 'error':
      return {
        _tag: 'Notice',
        id: entry.id,
        kind: 'error',
        text: entry.body.message,
        detail: entry.body.code,
        retryable: entry.body.retryable,
        at: entryTimestamp(entry),
      }
    case 'redaction':
      return {
        _tag: 'Notice',
        id: entry.id,
        kind: 'redaction',
        text: `Withheld ${entry.body.withheld_bytes.toLocaleString('en-US')} bytes`,
        detail: entry.body.reason,
        at: entryTimestamp(entry),
      }
    case 'truncation': {
      return {
        _tag: 'Notice',
        id: entry.id,
        kind: 'truncation',
        text: 'Older history unavailable in this transcript window',
        detail: `${entry.body.reason} · sequences ${entry.body.omitted_from_sequence}–${entry.body.omitted_to_sequence} omitted`,
        at: entryTimestamp(entry),
      }
    }
    case 'unrecognized':
      return {
        _tag: 'UnknownEvent',
        id: entry.id,
        eventType: entry.rawType,
        data: entry,
        ...(entry.timestamp !== undefined ? { at: entry.timestamp } : {}),
      }
  }
  return {
    _tag: 'UnknownEvent',
    id: entry.id,
    eventType: entry.type.raw,
    data: entry.body,
    at: entryTimestamp(entry),
  }
}

/** Convenience for fixtures and one-shot reads, using the same fold as live chunks. */
export const timelineItems = (
  frames: ReadonlyArray<FixtureConversationChunk>,
): ReadonlyArray<ConversationItem> => {
  const timeline = new LiveTimeline()
  for (const frame of frames) timeline.apply(frame)
  return timeline.project().items
}

interface CachedItem {
  readonly entry: Entry
  readonly result: ToolResultEntry | undefined
  readonly active: boolean
  readonly item: ConversationItem
}

/** One projection of a `LiveTimeline`: items before `changedFrom` are the previous objects. */
export interface TimelineProjection {
  readonly items: ReadonlyArray<ConversationItem>
  readonly changedFrom: number
}

/**
 * Mutable identity-stable fold of SDK-decoded conversation chunks.
 *
 * Live frames almost only touch the tail: the streaming entry's next revision, appended entries,
 * a result joining a recent call. `apply` records the lowest touched position and `project`
 * re-projects only from there, so a frame costs O(frame + suffix), not O(session). Anything that
 * replaces history (a replace page, an entry changing type) or flips session activity (every
 * unanswered call becomes `interrupted`) re-projects in full. Items keep their object identity
 * while their entry, joined result and activity input are unchanged.
 *
 * Entries keep the order st delivered them in. Each replace page arrives ordered by its own
 * projection's rule (the native page by `(timestamp, sequence)`, the stored fallback by
 * `sequence`), but a chunk names neither its projection nor where a delta entry merges into the
 * window, so the fold imposes no ordering of its own: deltas append, revisions stay in place,
 * and the next replace page is authoritative.
 */
export class LiveTimeline {
  private readonly entries = new Map<string, Entry>()
  private ordered: Array<Entry> = []
  /** First position of each call identity: used only to invalidate its projected items. */
  private readonly calls = new Map<string, number>()
  /**
   * Newest result per call identity (CAG.CLI.WEB.CNV-R04), even before its call is loaded.
   * `firstAt` is the earliest orphan row to invalidate when that call arrives.
   */
  private readonly results = new Map<
    string,
    { readonly entry: ToolResultEntry; readonly firstAt: number }
  >()
  /** Item count before each position in `ordered`, valid below `dirtyFrom`. */
  private itemsBefore: Array<number> = []
  private items: Array<ConversationItem> = []
  private readonly cache = new Map<string, CachedItem>()
  private active = true
  private dirtyFrom = 0
  private reindex = false
  /** Only a replace page describes the older-history edge. */
  hasOlder = false
  /** Native replace-page evidence, never an empty filtered projection. */
  observation: { readonly empty: boolean } | undefined

  get size(): number {
    return this.entries.size
  }

  /** Applies one follow frame (CAG.CLI.WEB.CNV-R01/R02); returns whether anything changed. */
  apply(frame: FixtureConversationChunk): boolean {
    let changed = frame.replace
    if (frame.replace) {
      this.entries.clear()
      this.ordered = []
      this.cache.clear()
      this.reindex = true
    }
    if (frame.replace) {
      this.hasOlder = frame.hasMore
      this.observation = frame.observation
    } else if (
      (frame.entries.length > 0 || frame.observation?.empty === false) &&
      this.observation?.empty === true
    ) {
      this.observation = { empty: false }
    }
    for (const entry of frame.entries) {
      const previous = this.entries.get(entry.id)
      if (previous !== undefined && previous.revision >= entry.revision) continue
      this.entries.set(entry.id, entry)
      changed = true
      // Mail may arrive after its native delivery; visibility depends on the whole shown set.
      if (entry.type === 'message' || previous?.type === 'message') this.dirtyFrom = 0
      // A later mailbox content entry may absorb a header already rendered in an earlier frame.
      if (entry.type === 'content' && MAILBOX_CONTENT_ID.test(entry.id)) this.dirtyFrom = 0
      let at: number
      if (previous === undefined) {
        at = this.ordered.push(entry) - 1
      } else {
        // Revisions almost always touch the streaming tail, so the backward scan is short.
        at = this.ordered.lastIndexOf(previous)
        this.ordered[at] = entry
        if (previous.type !== entry.type) {
          this.reindex = true
          continue
        }
      }
      this.dirtyFrom = Math.min(this.dirtyFrom, at)
      // Re-pointed identities or result sequence changes invalidate identity/result selection.
      if (
        previous !== undefined &&
        (entry.type === 'tool_call' || entry.type === 'tool_result') &&
        previous.type === entry.type &&
        (previous.body.call_id !== entry.body.call_id ||
          (entry.type === 'tool_result' && previous.sequence !== entry.sequence))
      ) {
        this.reindex = true
      }
      if (this.reindex) continue
      if (entry.type === 'tool_call' && previous === undefined) this.indexCall(entry, at)
      if (entry.type === 'tool_result') this.indexResult(entry, at)
    }
    return changed
  }

  /** Index a call by identity and remove any already-projected orphan results for it. */
  private indexCall(entry: Extract<Entry, { type: 'tool_call' }>, at: number): void {
    if (!this.calls.has(entry.body.call_id)) this.calls.set(entry.body.call_id, at)
    const result = this.results.get(entry.body.call_id)
    if (result !== undefined) this.dirtyFrom = Math.min(this.dirtyFrom, result.firstAt)
  }

  /** Retain results independently of call arrival order; sequence selects the newest result. */
  private indexResult(entry: ToolResultEntry, at: number): void {
    const id = entry.body.call_id
    const current = this.results.get(id)
    this.results.set(id, {
      entry:
        current === undefined || current.entry.id === entry.id || current.entry.sequence <= entry.sequence
          ? entry
          : current.entry,
      firstAt: Math.min(at, current?.firstAt ?? at),
    })
    const call = this.calls.get(id)
    if (call !== undefined) this.dirtyFrom = Math.min(this.dirtyFrom, call)
  }

  /** Message identities the window currently shows, without consuming projection state. */
  shownMessageIds(): Set<string> {
    const shown = new Set<string>()
    for (const entry of this.ordered)
      if (entry.type === 'message' && !isTurnHeader(entry)) shown.add(entry.body.message_id)
    return shown
  }

  /** Re-projects from the lowest touched position; free when nothing changed. */
  project(): TimelineProjection {
    if (this.reindex) this.rebuildIndex()
    // Session activity decides `running` vs `interrupted` for every unanswered call.
    const active = isActive(this.ordered.findLast((entry) => entry.type === 'status'))
    if (active !== this.active) {
      this.active = active
      this.dirtyFrom = 0
    }
    const from = Math.min(this.dirtyFrom, this.ordered.length)
    if (from === this.ordered.length && this.itemsBefore.length === this.ordered.length) {
      return { items: this.items, changedFrom: this.items.length }
    }
    const changedFrom = this.itemsBefore[from] ?? this.items.length
    const items = this.items.slice(0, changedFrom)
    this.itemsBefore.length = from
    const shown = this.shownMessageIds()
    const userMailContent = new Set<string>()
    for (const entry of this.ordered) {
      if (entry.type !== 'content' || entry.role !== 'user' || entry.body.media_type !== 'text/plain') continue
      const key = mailboxPairKey(entry)
      if (key !== undefined) userMailContent.add(key)
    }
    for (let at = from; at < this.ordered.length; at += 1) {
      const entry = this.ordered[at]!
      this.itemsBefore.push(items.length)
      if (isTurnHeader(entry)) continue
      if (entry.type === 'message' && entry.role === 'user' && entry.body.from?.startsWith('person/')) {
        const key = mailboxPairKey(entry)
        // Keep header-only mail and separate agent mail; one person send needs only its user row.
        if (key !== undefined && userMailContent.has(key)) continue
      }
      // Only native harness turns repeat shown mail as delivery copies; a mailbox
      // message's own content is person-authored text that may quote anything.
      if (
        entry.type === 'content' &&
        !MAILBOX_CONTENT_ID.test(entry.id) &&
        (entry.role === 'user' || entry.role === 'system')
      ) {
        const text = withoutShownDeliveries(entry.body.text ?? '', shown)
        if (text !== (entry.body.text ?? '')) {
          if (text.length > 0)
            items.push(itemOf({ entry: { ...entry, body: { ...entry.body, text } }, sessionActive: true }))
          continue
        }
      }
      if (entry.type === 'tool_result') {
        // A result is orphaned only when its call identity is absent, not when it arrived first.
        if (!this.calls.has(entry.body.call_id))
          items.push(this.cached({ entry, result: undefined, active: true }))
        continue
      }
      if (entry.type === 'tool_call')
        items.push(this.cached({ entry, result: this.results.get(entry.body.call_id)?.entry, active: this.active }))
      else items.push(this.cached({ entry, result: undefined, active: true }))
    }
    this.items = items
    this.dirtyFrom = this.ordered.length
    return { items, changedFrom }
  }

  /** Rebuild identity joins after history is replaced or an entry changes type/call identity. */
  private rebuildIndex(): void {
    this.reindex = false
    this.dirtyFrom = 0
    this.itemsBefore = []
    this.items = []
    this.calls.clear()
    this.results.clear()
    this.ordered.forEach((entry, at) => {
      if (entry.type === 'tool_call') this.indexCall(entry, at)
      else if (entry.type === 'tool_result') this.indexResult(entry, at)
    })
  }

  private cached({
    entry,
    result,
    active,
  }: {
    readonly entry: Entry
    readonly result: ToolResultEntry | undefined
    readonly active: boolean
  }): ConversationItem {
    const hit = this.cache.get(entry.id)
    if (hit !== undefined && hit.entry === entry && hit.result === result && hit.active === active)
      return hit.item
    let item: ConversationItem
    if (entry.type === 'tool_result') item = joinResult({ call: undefined, entry })
    else {
      const own = itemOf({ entry, sessionActive: active })
      item =
        result !== undefined && own._tag === 'ToolCall'
          ? joinResult({ call: own, entry: result })
          : own
    }
    this.cache.set(entry.id, { entry, result, active, item })
    return item
  }
}
