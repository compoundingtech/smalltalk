import type { Arrangement, ArrangementPage, CollectionStream, CollectionStreamOptions, EnvelopeOf, Snapshot } from '@smalltalk/st3-client'
import { afterEach, describe, expect, it, vi } from 'vitest'

import {
  followArrangementInventory,
  readArrangementInventory,
  sidebarWinner,
  type ArrangementInventoryGateway,
  type InventoryEvent,
} from './arrangements.ts'

const owner = 'person/example'
const snapshot: Snapshot = { id: 'snapshot/example/7/proof', host_id: 'host/example', store_index: 7, projection_version: 'client-projection.v0', created_at: '2026-01-01T00:00:00Z' }
const uuid = (n: number): string => `00000000-0000-7000-8000-${n.toString(16).padStart(12, '0')}`
const arrangement = (n: number, name = `Arrangement ${n}`): Arrangement => ({
  id: `arrangement/${owner}/${uuid(n)}`,
  owner,
  kind: 'arrangement',
  revision: `claim/${n}`,
  updated_at: '2026-01-01T00:00:00Z',
  deleted: false,
  body: { version: 1, name: { value: name, revision: `claim/${n}` }, folders: {}, placements: {} },
})
const page = (items: readonly Arrangement[], next?: string): ArrangementPage => ({
  kind: 'page',
  collection: 'arrangements',
  filters: { person: owner },
  items: [...items],
  page: { limit: 100, has_more: next !== undefined, next_cursor: next ?? null, cursor_expires_at: null },
})
const subscriptionId = 'arrangements-inventory'
const changes = (overrides: Record<string, unknown> = {}) => ({ kind: 'changes', id: subscriptionId, collection: 'arrangements', has_more: true, upserts: [], removes: [], order: [], snapshot, ...overrides })
const initial = { kind: 'snapshot', id: subscriptionId, collection: 'arrangements', has_more: true, items: [], order: [], snapshot }

/** One fake owner: `respond` answers each list page; sockets are recorded per open. */
const harness = () => {
  const calls: (string | undefined)[] = []
  const sockets: { options: CollectionStreamOptions; subscribed: unknown[][]; closed: number }[] = []
  const events: InventoryEvent[] = []
  let respond: (cursor: string | undefined) => Promise<ArrangementPage> = async () => page([])
  const gateway: ArrangementInventoryGateway = {
    arrangementsList: async (person, options = {}) => {
      expect(person).toBe(owner)
      calls.push(options.cursor)
      const value = await respond(options.cursor)
      return { api_version: 'st3.client.v0', request_id: 'request/test', snapshot, value } as EnvelopeOf<ArrangementPage>
    },
    collectionStream: async (options) => {
      const socket = { options, subscribed: [] as unknown[][], closed: 0 }
      sockets.push(socket)
      const stream: CollectionStream = {
        subscribeGlasses: () => undefined,
        subscribeArrangements: (...args) => { socket.subscribed.push(args) },
        subscribe: () => undefined,
        subscribeTerminal: () => undefined,
        subscribeConversation: () => undefined,
        unsubscribe: () => undefined,
        close: () => { socket.closed++ },
      }
      return stream
    },
  }
  const follow = followArrangementInventory({ gateway, owner, onEvent: (event) => events.push(event) })
  const frame = (value: unknown, socket = sockets.length - 1): void =>
    sockets[socket]!.options.onFrame(value as Parameters<CollectionStreamOptions['onFrame']>[0])
  const names = () => events.map((event) => event._tag === 'Complete' ? event.inventory.items.map((item) => item.body.name.value) : event._tag)
  return {
    calls, sockets, events, follow, frame, names,
    answer: (next: typeof respond) => { respond = next },
    opened: () => vi.waitFor(() => expect(sockets.at(-1)?.subscribed.length).toBe(1)),
  }
}

afterEach(() => { vi.useRealTimers() })

describe('owner-wide arrangement inventory', () => {
  it('subscribes owner-wide without a subject and paginates the whole list after the initial snapshot', async () => {
    const h = harness()
    h.answer(async (cursor) => cursor === undefined ? page([arrangement(1)], 'cursor/2') : page([arrangement(2)]))
    await h.opened()
    expect(h.sockets[0]!.subscribed).toEqual([[subscriptionId, owner, 1]])
    expect(h.calls).toEqual([])
    h.frame(initial)
    await vi.waitFor(() => expect(h.events).toHaveLength(1))
    expect(h.calls).toEqual([undefined, 'cursor/2'])
    expect(h.names()).toEqual([['Arrangement 1', 'Arrangement 2']])
    h.follow.close()
  })

  it('re-paginates on an empty changes frame', async () => {
    const h = harness()
    let name = 'Before'
    h.answer(async (cursor) => cursor === undefined ? page([arrangement(1)], 'cursor/2') : page([arrangement(2, name)]))
    await h.opened()
    h.frame(initial)
    await vi.waitFor(() => expect(h.events).toHaveLength(1))
    name = 'Renamed outside the window'
    h.frame(changes())
    await vi.waitFor(() => expect(h.events).toHaveLength(2))
    expect(h.calls).toEqual([undefined, 'cursor/2', undefined, 'cursor/2'])
    expect(h.names().at(-1)).toEqual(['Arrangement 1', 'Renamed outside the window'])
    h.follow.close()
  })

  it('re-paginates on frames repeating the same snapshot index instead of deduplicating them', async () => {
    const h = harness()
    let items = [arrangement(2)]
    h.answer(async () => page(items))
    await h.opened()
    h.frame(initial)
    await vi.waitFor(() => expect(h.events).toHaveLength(1))
    for (const [index, next] of [[arrangement(1), arrangement(2)], [arrangement(1)]].entries()) {
      items = next
      // Identical frame, identical snapshot id and store_index as the initial snapshot.
      h.frame(changes())
      await vi.waitFor(() => expect(h.events).toHaveLength(index + 2))
    }
    expect(h.calls).toEqual([undefined, undefined, undefined])
    expect(h.names()).toEqual([['Arrangement 2'], ['Arrangement 1', 'Arrangement 2'], ['Arrangement 1']])
    h.follow.close()
  })

  it('coalesces invalidations during an in-flight read into one follow-up complete read', async () => {
    const h = harness()
    const reads: PromiseWithResolvers<ArrangementPage>[] = []
    h.answer(() => {
      const read = Promise.withResolvers<ArrangementPage>()
      reads.push(read)
      return read.promise
    })
    await h.opened()
    h.frame(initial)
    await vi.waitFor(() => expect(reads).toHaveLength(1))
    h.frame(changes())
    h.frame(changes())
    h.frame(changes({ upserts: [arrangement(1)], order: [arrangement(1).id] }))
    expect(reads).toHaveLength(1)
    reads[0]!.resolve(page([arrangement(2)]))
    await vi.waitFor(() => expect(reads).toHaveLength(2))
    // The socket stays open and keeps invalidating while the follow-up read runs.
    expect(h.sockets[0]!.closed).toBe(0)
    h.frame(changes())
    reads[1]!.resolve(page([arrangement(1), arrangement(2)]))
    await vi.waitFor(() => expect(reads).toHaveLength(3))
    reads[2]!.resolve(page([arrangement(1), arrangement(2)]))
    await vi.waitFor(() => expect(h.events).toHaveLength(3))
    // A pending follow-up would already have issued its first page synchronously after the event.
    expect(reads).toHaveLength(3)
    expect(h.names()).toEqual([['Arrangement 2'], ['Arrangement 1', 'Arrangement 2'], ['Arrangement 1', 'Arrangement 2']])
    h.follow.close()
  })

  it('never infers absence from the bounded window or a partial pagination', async () => {
    const h = harness()
    let failSecondPage = false
    h.answer(async (cursor) => {
      if (cursor === undefined) return page([arrangement(1)], 'cursor/2')
      if (failSecondPage) throw new Error('page 2 unavailable')
      return page([arrangement(2)])
    })
    await h.opened()
    // The bounded window omits every row; only the complete read decides membership.
    h.frame({ ...initial, items: [], order: [], has_more: true })
    await vi.waitFor(() => expect(h.events).toHaveLength(1))
    expect(h.names()).toEqual([['Arrangement 1', 'Arrangement 2']])
    failSecondPage = true
    h.frame(changes({ removes: [arrangement(2).id] }))
    await vi.waitFor(() => expect(h.events).toHaveLength(2))
    expect(h.events[1]).toMatchObject({ _tag: 'ReadFailed', error: { message: 'page 2 unavailable' } })
    expect(h.events.filter((event) => event._tag === 'Complete')).toHaveLength(1)
    h.follow.close()
  })

  it('fails a page that claims more rows without a cursor rather than publishing a prefix', async () => {
    const truncated = { ...page([arrangement(1)]), page: { limit: 100, has_more: true, next_cursor: null, cursor_expires_at: null } }
    await expect(readArrangementInventory({ arrangementsList: async () => ({ api_version: 'st3.client.v0', request_id: 'request/test', snapshot, value: truncated }) as EnvelopeOf<ArrangementPage> }, owner))
      .rejects.toThrow('continuation cursor')
    let repeated = 0
    await expect(readArrangementInventory({ arrangementsList: async () => { repeated++; return { api_version: 'st3.client.v0', request_id: 'request/test', snapshot, value: page([arrangement(1)], 'cursor/same') } as EnvelopeOf<ArrangementPage> } }, owner))
      .rejects.toThrow('repeated its cursor')
    expect(repeated).toBe(2)
    for (const foreign of [{ ...arrangement(1), owner: 'person/other' }, { ...arrangement(1), id: `arrangement/person/other/${uuid(1)}` }])
      await expect(readArrangementInventory({ arrangementsList: async () => ({ api_version: 'st3.client.v0', request_id: 'request/test', snapshot, value: page([foreign]) }) as EnvelopeOf<ArrangementPage> }, owner))
        .rejects.toThrow('owner-scoped')
  })

  it('ignores other subscriptions and resync notices, and stops on a permanent refusal until refreshed', async () => {
    const h = harness()
    h.answer(async () => page([arrangement(1)]))
    await h.opened()
    h.frame({ ...changes(), id: 'other' })
    h.frame({ kind: 'resync', id: subscriptionId, collection: 'arrangements', retryable: true })
    // Invalidations issue their first page synchronously, so no read means none was scheduled.
    expect(h.calls).toEqual([])
    h.frame({ kind: 'error', id: subscriptionId, collection: 'arrangements', message: 'forbidden', retryable: false })
    expect(h.names()).toEqual(['Refused'])
    expect(h.sockets[0]!.closed).toBe(1)
    h.frame(changes(), 0)
    expect(h.calls).toEqual([])
    h.follow.refresh()
    await h.opened()
    expect(h.sockets).toHaveLength(2)
    h.frame(initial)
    await vi.waitFor(() => expect(h.events).toHaveLength(2))
    h.follow.close()
  })

  it('reconnects after an unexpected end and re-reads from the new authoritative snapshot', async () => {
    vi.useFakeTimers()
    const h = harness()
    h.answer(async () => page([arrangement(1)]))
    await h.opened()
    h.sockets[0]!.options.onEnd?.(new Error('socket closed (1006)'))
    expect(h.events).toEqual([{ _tag: 'Interrupted', error: new Error('socket closed (1006)'), retryInMs: 1_000 }])
    await vi.advanceTimersByTimeAsync(999)
    expect(h.sockets).toHaveLength(1)
    await vi.advanceTimersByTimeAsync(1)
    await h.opened()
    expect(h.sockets).toHaveLength(2)
    // A late frame from the ended socket is ignored.
    h.frame(changes(), 0)
    h.frame(initial)
    await vi.waitFor(() => expect(h.events).toHaveLength(2))
    expect(h.calls).toEqual([undefined])
    h.follow.close()
    h.sockets[1]!.options.onEnd?.(new Error('after close'))
    expect(h.events).toHaveLength(2)
  })
})

describe('sidebarWinner', () => {
  it('selects the lowest UUIDv7 regardless of list order or name', () => {
    expect(sidebarWinner([arrangement(3, 'Sidebar'), arrangement(1, 'Renamed'), arrangement(2, 'Sidebar')])?.id).toBe(arrangement(1).id)
    expect(sidebarWinner([arrangement(0x10), arrangement(0x9)])?.id).toBe(arrangement(0x9).id)
    expect(sidebarWinner([])).toBeUndefined()
  })
})
