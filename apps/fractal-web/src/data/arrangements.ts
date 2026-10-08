/**
 * Owner-wide native arrangement inventory.
 *
 * The collections window is bounded by count and bytes, so it can never prove that an
 * arrangement is absent. It is used only as an invalidation signal: the initial snapshot and
 * EVERY owner-wide `changes` frame (including empty frames and frames that repeat the previous
 * snapshot index) trigger a complete re-pagination of the HTTP arrangements list. Invalidations
 * that arrive while a read is in flight coalesce into exactly one follow-up complete read.
 * Only a complete read is published; a partial pagination never replaces the last inventory.
 */
import type { Arrangement, CollectionSocketFactory, CollectionStream, St3Client } from '@smalltalk/st3-client'
import { ArrangementPage as ArrangementPageSchema } from '@smalltalk/st3-client/schema'
import { Schema } from 'effect'

const isArrangementPage = Schema.is(Schema.toEncoded(ArrangementPageSchema))
// Frames are invalidation signals: only the routing fields are read, never the window rows.
const isFrameHeader = Schema.is(Schema.Struct({
  kind: Schema.String,
  id: Schema.optionalKey(Schema.String),
  collection: Schema.optionalKey(Schema.String),
  message: Schema.optionalKey(Schema.String),
}))

export type ArrangementInventoryGateway = Pick<St3Client, 'arrangementsList' | 'collectionStream'>

/** Every live arrangement of one owner, read across all pages of one paginated list. */
export interface ArrangementInventory {
  readonly owner: string
  readonly items: readonly Arrangement[]
}

export type InventoryEvent =
  /** A complete re-pagination finished; `items` is the whole owner inventory. */
  | { readonly _tag: 'Complete'; readonly inventory: ArrangementInventory }
  /** A read failed and no follow-up read is pending; the last complete inventory still stands. */
  | { readonly _tag: 'ReadFailed'; readonly error: Error }
  /** The socket ended; a new socket subscribes again after `retryInMs`. */
  | { readonly _tag: 'Interrupted'; readonly error: Error; readonly retryInMs: number }
  /** The server permanently refused the subscription; only `refresh` reopens it. */
  | { readonly _tag: 'Refused'; readonly error: Error }

export interface InventoryFollow {
  /** Reopen an ended subscription now, or re-paginate while it is open. */
  readonly refresh: () => void
  readonly close: () => void
}

const subscriptionId = 'arrangements-inventory'
const asError = (error: unknown): Error => (error instanceof Error ? error : new Error(String(error)))

/**
 * The live Sidebar is the owner's arrangement with the lowest UUIDv7, independent of name or
 * page order. Inventory IDs are schema-checked lowercase UUIDv7s under one owner prefix, so ID
 * order is UUID (creation) order.
 */
export const sidebarWinner = (items: readonly Arrangement[]): Arrangement | undefined =>
  items.reduce<Arrangement | undefined>((winner, item) => (winner === undefined || item.id < winner.id ? item : winner), undefined)

/** Read every page; a page that claims more items without a fresh cursor fails the whole read. */
export const readArrangementInventory = async (
  gateway: Pick<ArrangementInventoryGateway, 'arrangementsList'>,
  owner: string,
): Promise<ArrangementInventory> => {
  const items: Arrangement[] = []
  const visited = new Set<string>()
  const prefix = `arrangement/${owner}/`
  let cursor: string | undefined
  for (;;) {
    const page = (await gateway.arrangementsList(owner, cursor === undefined ? {} : { cursor })).value
    if (!isArrangementPage(page) || page.items.some((item) => item.owner !== owner || !item.id.startsWith(prefix)))
      throw new TypeError('Invalid owner-scoped arrangements page')
    items.push(...page.items)
    if (!page.page.has_more) return { owner, items }
    const next = page.page.next_cursor
    if (next === null || next === undefined) throw new TypeError('Arrangement page omitted its continuation cursor')
    if (visited.has(next)) throw new TypeError('Arrangement pagination repeated its cursor')
    visited.add(next)
    cursor = next
  }
}

/** Follow one owner's complete inventory through an owner-wide subscription without `subject`. */
export const followArrangementInventory = ({
  gateway,
  owner,
  onEvent,
  socket,
}: {
  readonly gateway: ArrangementInventoryGateway
  readonly owner: string
  readonly onEvent: (event: InventoryEvent) => void
  /** Test seam for the collections WebSocket. */
  readonly socket?: CollectionSocketFactory
}): InventoryFollow => {
  let closed = false
  let generation = 0
  let stream: CollectionStream | undefined
  let timer: ReturnType<typeof setTimeout> | undefined
  let attempt = 0
  let reading = false
  let dirty = false

  const emit = (event: InventoryEvent): void => {
    if (!closed) onEvent(event)
  }
  // Complete reads run one at a time; any invalidation during a read schedules one more.
  const read = async (): Promise<void> => {
    reading = true
    try {
      do {
        dirty = false
        try {
          emit({ _tag: 'Complete', inventory: await readArrangementInventory(gateway, owner) })
        } catch (error) {
          if (!dirty) emit({ _tag: 'ReadFailed', error: asError(error) })
        }
      } while (dirty && !closed)
    } finally {
      reading = false
    }
  }
  const invalidate = (): void => {
    if (closed) return
    if (reading) dirty = true
    else void read()
  }
  const interrupted = (current: number, error: Error): void => {
    if (closed || current !== generation) return
    generation++
    stream?.close()
    stream = undefined
    // Exponential reconnect backoff: 1 s doubling per consecutive failure, capped at 30 s.
    const retryInMs = Math.min(30_000, 1_000 * 2 ** attempt++)
    emit({ _tag: 'Interrupted', error, retryInMs })
    timer = setTimeout(() => void open(), retryInMs)
  }
  const refused = (current: number, error: Error): void => {
    if (closed || current !== generation) return
    generation++
    stream?.close()
    stream = undefined
    emit({ _tag: 'Refused', error })
  }
  const open = async (): Promise<void> => {
    timer = undefined
    const current = ++generation
    let connection: CollectionStream
    try {
      connection = await gateway.collectionStream({
        ...(socket === undefined ? {} : { socket }),
        onFrame: (frame) => {
          if (current !== generation) return
          if (!isFrameHeader(frame)) {
            interrupted(current, new TypeError('Invalid arrangements collection frame'))
            return
          }
          if (frame.id !== subscriptionId) return
          if (frame.kind === 'error') {
            refused(current, new Error(frame.message ?? 'Arrangement subscription refused'))
            return
          }
          if ((frame.kind !== 'snapshot' && frame.kind !== 'changes') || frame.collection !== 'arrangements') return
          // A new socket's snapshot is authoritative again; the reconnect budget starts over.
          if (frame.kind === 'snapshot') attempt = 0
          invalidate()
        },
        onEnd: (error) => interrupted(current, error ?? new Error('The arrangements socket closed')),
      })
    } catch (error) {
      interrupted(current, asError(error))
      return
    }
    if (closed || current !== generation) {
      connection.close()
      return
    }
    stream = connection
    // The window is only an invalidation signal, so one row bounds each frame's payload.
    connection.subscribeArrangements(subscriptionId, owner, 1)
  }

  void open()
  return {
    refresh: () => {
      if (closed) return
      if (stream !== undefined) {
        invalidate()
        return
      }
      clearTimeout(timer)
      attempt = 0
      void open()
    },
    close: () => {
      closed = true
      generation++
      clearTimeout(timer)
      stream?.close()
      stream = undefined
    },
  }
}
