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
 * reorders history (an older page, a sequence change) or flips session activity (every
 * unanswered call becomes `interrupted`) re-projects in full. Items keep their object identity
 * while their entry, joined result and activity input are unchanged.
 */
export class LiveTimeline {
  private readonly entries = new Map<string, Entry>()
  private ordered: Array<Entry> = []
  /** Positions in `ordered` of every `tool_call`, per call id, ascending. */
  private readonly calls = new Map<string, Array<number>>()
  /**
   * Joined result per call position: the newest result after that call and before the next call
   * with the same id (CAG.CLI.WEB.CNV-R04).
   */
  private readonly joined = new Map<
    number,
    { readonly entry: ToolResultEntry; readonly at: number }
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
      const last = this.ordered.at(-1)
      let at: number
      if (
        previous !== undefined &&
        previous.sequence === entry.sequence &&
        previous.type === entry.type
      ) {
        at = this.positionOf(previous)
        this.ordered[at] = entry
      } else if (
        previous === undefined &&
        (last === undefined || last.sequence <= entry.sequence)
      ) {
        at = this.ordered.push(entry) - 1
      } else {
        if (previous !== undefined) this.ordered.splice(this.positionOf(previous), 1)
        this.ordered.splice(this.insertionIndex(entry.sequence), 0, entry)
        this.reindex = true
        continue
      }
      this.dirtyFrom = Math.min(this.dirtyFrom, at)
      // A revision that re-points a call or result to another call id invalidates the join index.
      if (
        previous !== undefined &&
        (entry.type === 'tool_call' || entry.type === 'tool_result') &&
        previous.type === entry.type &&
        previous.body.call_id !== entry.body.call_id
      ) {
        this.reindex = true
      }
      if (this.reindex) continue
      if (entry.type === 'tool_call' && previous === undefined) {
        const positions = this.calls.get(entry.body.call_id)
        if (positions === undefined) this.calls.set(entry.body.call_id, [at])
        else positions.push(at)
      }
      if (entry.type === 'tool_result') this.join({ entry, at })
    }
    return changed
  }

  /** Joins a result at `at` into the latest same-id call before it, if it is the newest there. */
  private join({ entry, at }: { readonly entry: ToolResultEntry; readonly at: number }) {
    const positions = this.calls.get(entry.body.call_id)
    if (positions === undefined) return
    let call: number | undefined
    for (let index = positions.length - 1; index >= 0; index -= 1) {
      if (positions[index]! < at) {
        call = positions[index]!
        break
      }
    }
    if (call === undefined) return
    const current = this.joined.get(call)
    if (current !== undefined && current.at > at) return
    this.joined.set(call, { entry, at })
    this.dirtyFrom = Math.min(this.dirtyFrom, call)
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
    const shown = new Set(
      this.ordered.flatMap((entry) =>
        entry.type === 'message' && !isTurnHeader(entry) ? [entry.body.message_id] : [],
      ),
    )
    for (let at = from; at < this.ordered.length; at += 1) {
      const entry = this.ordered[at]!
      this.itemsBefore.push(items.length)
      if (isTurnHeader(entry)) continue
      // Only native harness turns repeat shown mail as delivery copies; a mailbox
      // message's own content is person-authored text that may quote anything.
      const mailboxPair = at > 0 && this.ordered[at - 1]!.type === 'message' && !isTurnHeader(this.ordered[at - 1]!)
      if (entry.type === 'content' && !mailboxPair && (entry.role === 'user' || entry.role === 'system')) {
        const text = withoutShownDeliveries(entry.body.text ?? '', shown)
        if (text !== (entry.body.text ?? '')) {
          if (text.length > 0)
            items.push(itemOf({ entry: { ...entry, body: { ...entry.body, text } }, sessionActive: true }))
          continue
        }
      }
      if (entry.type === 'tool_result') {
        // A result after any same-id call folds into a call (the newest one wins there).
        const first = this.calls.get(entry.body.call_id)?.[0]
        if (first === undefined || first > at)
          items.push(this.cached({ entry, result: undefined, active: true }))
        continue
      }
      if (entry.type === 'tool_call')
        items.push(this.cached({ entry, result: this.joined.get(at)?.entry, active: this.active }))
      else items.push(this.cached({ entry, result: undefined, active: true }))
    }
    this.items = items
    this.dirtyFrom = this.ordered.length
    return { items, changedFrom }
  }

  /** Recomputes the call/result join after history was replaced or reordered. */
  private rebuildIndex(): void {
    this.reindex = false
    this.dirtyFrom = 0
    this.itemsBefore = []
    this.items = []
    this.calls.clear()
    this.joined.clear()
    this.ordered.forEach((entry, at) => {
      if (entry.type === 'tool_call') {
        const positions = this.calls.get(entry.body.call_id)
        if (positions === undefined) this.calls.set(entry.body.call_id, [at])
        else positions.push(at)
      } else if (entry.type === 'tool_result') this.join({ entry, at })
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

  private insertionIndex(sequence: number): number {
    let low = 0
    let high = this.ordered.length
    while (low < high) {
      const mid = (low + high) >>> 1
      if (this.ordered[mid]!.sequence <= sequence) low = mid + 1
      else high = mid
    }
    return low
  }

  /** Position of an entry already in `ordered`; searches back from its sequence slot. */
  private positionOf(entry: Entry): number {
    for (let at = this.insertionIndex(entry.sequence) - 1; at >= 0; at -= 1) {
      if (this.ordered[at]!.id === entry.id) return at
      if (this.ordered[at]!.sequence < entry.sequence) break
    }
    return this.ordered.findIndex((candidate) => candidate.id === entry.id)
  }
}
