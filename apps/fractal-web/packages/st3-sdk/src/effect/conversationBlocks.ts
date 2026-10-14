// Per-block decode of st's conversation entries. The generated `TimelineEntry` nests `blocks`, so one
// malformed view (st 84c3622 sends float `wall_ms` on bash outputs) would reject the whole entry.
// The entry decodes with its blocks lifted out, then every block decodes on its own, then a failing
// block retries without its view: the smallest unit that fails is the unit that degrades.
import { TimelineBlock, TimelineEntry, type UnknownCase } from '@smalltalk/st3-client/schema'
import { Exit, Schema } from 'effect'

/** What one block lost while decoding. */
export type BlockDegrade =
  /** The block decoded, but its view failed its closed definition or names a future view type; st's
   * contract renders such a block as without a view, so the block is kept without it. */
  | { readonly _tag: 'ViewDropped'; readonly blockId: string; readonly kind: string; readonly viewType: string; readonly issue: string }
  /** The block itself failed; it is left out of `body.blocks` and kept for an Unsupported marker. */
  | {
      readonly _tag: 'BlockDropped'
      readonly blockId: string | undefined
      readonly kind: string | undefined
      readonly sourceType: string | undefined
      readonly issue: string
      readonly raw: unknown
    }

/** A schema-decoded entry with the blocks that degraded while decoding it. */
export type ConversationEntry = TimelineEntry & { readonly degraded?: ReadonlyArray<BlockDegrade> }

/** The outcome of decoding one wire entry; never throws. */
export type EntryDecode =
  | { readonly _tag: 'Entry'; readonly entry: ConversationEntry }
  | { readonly _tag: 'BadEntry'; readonly issue: string }

const tolerant = { onExcessProperty: 'ignore' } as const
const decodeTimelineEntry = Schema.decodeUnknownExit(TimelineEntry, tolerant)
const decodeTimelineBlock = Schema.decodeUnknownExit(TimelineBlock, tolerant)

type Json = { readonly [key: string]: unknown }
const isJson = (value: unknown): value is Json => typeof value === 'object' && value !== null && !Array.isArray(value)
const stringAt = (value: Json, key: string): string | undefined => {
  const field = value[key]
  return typeof field === 'string' ? field : undefined
}
const issueOf = (exit: Exit.Exit<unknown, unknown>): string => (Exit.isFailure(exit) ? String(exit.cause).slice(0, 2_000) : '')
const isFutureCase = (value: string | UnknownCase): value is UnknownCase => typeof value !== 'string'

type BlockDecode =
  | { readonly _tag: 'Kept'; readonly block: TimelineBlock; readonly degrade?: BlockDegrade }
  | { readonly _tag: 'Dropped'; readonly degrade: BlockDegrade }

const decodeBlock = (raw: unknown): BlockDecode => {
  const exit = decodeTimelineBlock(raw)
  if (Exit.isSuccess(exit)) {
    const { view, ...block } = exit.value
    if (view === undefined || !isFutureCase(view.type)) return { _tag: 'Kept', block: exit.value }
    return {
      _tag: 'Kept', block,
      degrade: { _tag: 'ViewDropped', blockId: block.id, kind: block.kind, viewType: view.type.raw, issue: 'future view type' },
    }
  }
  if (isJson(raw) && isJson(raw['view'])) {
    const { view, ...withoutView } = raw
    const retry = decodeTimelineBlock(withoutView)
    if (Exit.isSuccess(retry)) {
      return {
        _tag: 'Kept', block: retry.value,
        degrade: { _tag: 'ViewDropped', blockId: retry.value.id, kind: retry.value.kind, viewType: stringAt(view, 'type') ?? 'untyped', issue: issueOf(exit) },
      }
    }
  }
  return {
    _tag: 'Dropped',
    degrade: {
      _tag: 'BlockDropped',
      blockId: isJson(raw) ? stringAt(raw, 'id') : undefined,
      kind: isJson(raw) ? stringAt(raw, 'kind') : undefined,
      sourceType: isJson(raw) ? stringAt(raw, 'source_type') : undefined,
      issue: issueOf(exit),
      raw,
    },
  }
}

/** Decode one wire entry: a failing block degrades that block, never its entry. */
export const decodeConversationEntry = (raw: unknown): EntryDecode => {
  const body = isJson(raw) && isJson(raw['body']) ? raw['body'] : undefined
  const rawBlocks = body?.['blocks']
  const lifted = !isJson(raw) || body === undefined || !('blocks' in body) ? raw : { ...raw, body: withoutBlocks(body) }
  const exit = decodeTimelineEntry(lifted)
  if (Exit.isFailure(exit)) return { _tag: 'BadEntry', issue: issueOf(exit) }
  const entry = exit.value
  if (!Array.isArray(rawBlocks)) return { _tag: 'Entry', entry }
  const blocks: Array<TimelineBlock> = []
  const degraded: Array<BlockDegrade> = []
  for (const rawBlock of rawBlocks) {
    const decoded = decodeBlock(rawBlock)
    if (decoded._tag === 'Kept') blocks.push(decoded.block)
    if (decoded.degrade !== undefined) degraded.push(decoded.degrade)
  }
  // Every generated body that carries blocks types them as `TimelineBlock[]`; the unknown entry
  // fallback keeps its body as an open record, where the decoded blocks are equally valid.
  Object.assign(entry.body, { blocks })
  return { _tag: 'Entry', entry: degraded.length === 0 ? entry : { ...entry, degraded } }
}

const withoutBlocks = (body: Json): Json => {
  const { blocks: _blocks, ...rest } = body
  return rest
}
