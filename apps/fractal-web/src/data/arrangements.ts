/**
 * Owner-wide native arrangement inventory.
 *
 * The collections window is bounded by count and bytes, so it can never prove that an
 * arrangement is absent. It is used only as an invalidation signal: the initial snapshot and
 * EVERY owner-wide `changes` frame (including empty frames and frames that repeat the previous
 * snapshot index) trigger a complete re-pagination of the HTTP arrangements list. Invalidations
 * that arrive while a read is in flight coalesce into exactly one follow-up complete read.
 * Only a complete read is published; a partial pagination never replaces the last inventory.
 * A complete read is finite: it stops at fixed page and item budgets, and closing or refreshing
 * the follow aborts it between pages and cancels its in-flight request.
 */
import { St3Client } from '@smalltalk/st3-client'
import type { Arrangement, ArrangementPage, CollectionSocketFactory, CollectionStream, EnvelopeOf, PageOptions } from '@smalltalk/st3-client'
import { ArrangementId as ArrangementIdSchema, ArrangementPage as ArrangementPageSchema } from '@smalltalk/st3-client/schema'
import { Schema } from 'effect'

const isArrangementPage = Schema.is(Schema.toEncoded(ArrangementPageSchema))
// Generated Arrangement.id is a generic subject Id; the winner rule needs the full UUIDv7 form.
const isArrangementId = Schema.is(Schema.toEncoded(ArrangementIdSchema))
// Frames are invalidation signals: only the routing fields are read, never the window rows.
const isFrameHeader = Schema.is(Schema.Struct({
  kind: Schema.String,
  id: Schema.optionalKey(Schema.String),
  collection: Schema.optionalKey(Schema.String),
  message: Schema.optionalKey(Schema.String),
}))

export interface ArrangementInventoryGateway {
  /** One list page; `signal` cancels the request when the read is abandoned. */
  readonly arrangementsList: (person: string, options: PageOptions, signal: AbortSignal) => Promise<EnvelopeOf<ArrangementPage>>
  readonly collectionStream: St3Client['collectionStream']
}

/**
 * The generated client for ONE follow. A follow runs one list request at a time, so the
 * request being sent belongs to the most recent read's signal; never share it between follows.
 */
export const st3InventoryGateway = ({ baseUrl, fetchImpl }: {
  readonly baseUrl: string
  readonly fetchImpl: typeof globalThis.fetch
}): ArrangementInventoryGateway & Pick<St3Client, 'discover'> => {
  let signal: AbortSignal | undefined
  const client = new St3Client({
    baseUrl,
    fetchImpl: (input, init) => fetchImpl(input, signal === undefined ? init : { ...init, signal }),
  })
  return {
    discover: () => client.discover(),
    collectionStream: (options) => client.collectionStream(options),
    arrangementsList: (person, options, read) => {
      signal = read
      return client.arrangementsList(person, options)
    },
  }
}

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

/** Rows requested per page; a page returning more is malformed. */
export const inventoryPageLimit = 100
/**
 * Whole-read budget. Local admission allows 100 live arrangements per person; replicated unions
 * may exceed that, so the budget leaves headroom but stays finite. Every non-final page must
 * carry at least one row, so the item budget also bounds the page count.
 */
export const inventoryItemBudget = 1_000

/** Read every page; any malformed, foreign, non-advancing or over-budget page fails the whole read. */
export const readArrangementInventory = async (
  gateway: Pick<ArrangementInventoryGateway, 'arrangementsList'>,
  owner: string,
  signal: AbortSignal,
): Promise<ArrangementInventory> => {
  const items: Arrangement[] = []
  const visited = new Set<string>()
  const prefix = `arrangement/${owner}/`
  // Abandon a request at once even if its transport ignores the signal.
  const abandoned = Promise.withResolvers<never>()
  const onAbort = () => abandoned.reject(signal.reason)
  signal.addEventListener('abort', onAbort, { once: true })
  try {
    let cursor: string | undefined
    for (;;) {
      signal.throwIfAborted()
      const options = cursor === undefined ? { limit: inventoryPageLimit } : { cursor, limit: inventoryPageLimit }
      const page = (await Promise.race([gateway.arrangementsList(owner, options, signal), abandoned.promise])).value
      if (!isArrangementPage(page) || page.items.some((item) => item.owner !== owner || !item.id.startsWith(prefix) || !isArrangementId(item.id)))
        throw new TypeError('Invalid owner-scoped arrangements page')
      if (page.items.length > inventoryPageLimit)
        throw new TypeError(`Arrangement page returned ${page.items.length} rows for a limit of ${inventoryPageLimit}`)
      if (items.length + page.items.length > inventoryItemBudget)
        throw new RangeError(`Arrangement inventory exceeds ${inventoryItemBudget} arrangements`)
      items.push(...page.items)
      if (!page.page.has_more) return { owner, items }
      if (page.items.length === 0) throw new TypeError('Arrangement pagination did not advance')
      const next = page.page.next_cursor
      if (next === null || next === undefined) throw new TypeError('Arrangement page omitted its continuation cursor')
      if (visited.has(next)) throw new TypeError('Arrangement pagination repeated its cursor')
      visited.add(next)
      cursor = next
    }
  } finally {
    signal.removeEventListener('abort', onAbort)
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
  let pending: AbortController | undefined

  const emit = (event: InventoryEvent): void => {
    if (!closed) onEvent(event)
  }
  // Complete reads run one at a time; any invalidation during a read schedules one more.
  const read = async (): Promise<void> => {
    reading = true
    try {
      do {
        dirty = false
        const current = new AbortController()
        pending = current
        try {
          const inventory = await readArrangementInventory(gateway, owner, current.signal)
          if (!current.signal.aborted) emit({ _tag: 'Complete', inventory })
        } catch (error) {
          if (!dirty && !current.signal.aborted) emit({ _tag: 'ReadFailed', error: asError(error) })
        }
      } while (dirty && !closed)
    } finally {
      reading = false
      pending = undefined
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
        // An explicit refresh abandons the running read for a fresh one.
        if (reading) {
          dirty = true
          pending?.abort()
        } else void read()
        return
      }
      // The socket ended or was refused mid-read: abandon that read; the new snapshot starts a fresh one.
      pending?.abort()
      clearTimeout(timer)
      attempt = 0
      void open()
    },
    close: () => {
      closed = true
      generation++
      pending?.abort()
      clearTimeout(timer)
      stream?.close()
      stream = undefined
    },
  }
}
