import { Agent } from '@smalltalk/st3-client/schema'
import { Schema } from 'effect'

import { ConversationItem } from '../conversation/model.ts'
import type { ConversationPage } from './source.ts'

/** Only previously decoded read projections; connection, grants and capabilities never enter this cache. */
export interface CachedProjection<A> {
  readonly read: () => Promise<A | undefined>
  readonly write: (value: A) => void
  readonly remove: () => Promise<void>
}
/** Bounded retained read observations and their permission-revocation lifecycle. */
export interface ReloadCache {
  readonly agents: CachedProjection<readonly Agent[]>
  readonly conversation: (ref: string) => CachedProjection<ConversationPage>
  readonly clear: () => Promise<void>
  readonly dispose: () => Promise<void>
}

/** Disposable observations, bounded across every gateway sharing the browser origin. */
export const reloadCacheLimits = {
  ageMs: 24 * 60 * 60 * 1000,
  agents: 100,
  conversations: 12,
  items: 256,
  recordBytes: 512 * 1024,
  totalBytes: 4 * 1024 * 1024,
  records: 32,
} as const

const databaseName = 'wf.observed@1'
const storeName = 'projections'
const revocationStore = 'revocations'
const Revocation = Schema.Struct({ gateway: Schema.String, key: Schema.String, at: Schema.Int })
const decodeRevocations = (raw: unknown) => {
  try {
    return Schema.decodeUnknownSync(Schema.Array(Revocation))(raw)
  } catch {
    return undefined
  }
}
const Stored = Schema.Struct({
  version: Schema.Literal(1),
  gateway: Schema.String,
  key: Schema.String,
  writtenAt: Schema.Int,
  payload: Schema.String,
})
type Stored = typeof Stored.Type
const decodeStored = Schema.decodeUnknownSync(Stored)
const AgentsJson = Schema.fromJsonString(Schema.Array(Agent))
const PageJson = Schema.fromJsonString(
  Schema.Struct({ items: Schema.Array(ConversationItem), hasOlder: Schema.Boolean }),
)
const ItemJson = Schema.fromJsonString(ConversationItem)
const encodeAgents = Schema.encodeSync(AgentsJson)
const encodePage = Schema.encodeSync(PageJson)
const encodeItem = Schema.encodeSync(ItemJson)
const bytes = (record: Stored) => 2 * record.payload.length
const validRecord = ({
  raw,
  now,
}: {
  readonly raw: unknown
  readonly now: number
}): Stored | undefined => {
  try {
    const record = decodeStored(raw)
    return record.writtenAt <= now &&
      now - record.writtenAt < reloadCacheLimits.ageMs &&
      bytes(record) <= reloadCacheLimits.recordBytes
      ? record
      : undefined
  } catch {
    return undefined
  }
}

const boundedPage = (page: ConversationPage): ConversationPage => {
  // Measure only the retained tail, on the background write task, not on a frame/render path.
  const tail = page.items.slice(-reloadCacheLimits.items)
  let size = 128
  let start = tail.length
  while (start > 0) {
    const next = 2 * encodeItem(tail[start - 1]!).length + 2
    if (size + next > reloadCacheLimits.recordBytes) break
    size += next
    start -= 1
  }
  return {
    items: tail.slice(start),
    hasOlder: page.hasOlder || start > 0 || tail.length < page.items.length,
  }
}

/** Explicit browser capability injection keeps SSR and node tests independent of ambient IndexedDB. */
export const createReloadCache = ({
  indexedDB,
  gateway,
  now = Date.now,
}: {
  readonly indexedDB: IDBFactory
  readonly gateway: string
  readonly now?: () => number
}): ReloadCache => {
  // Never store URL credentials, query tokens or fragments as namespace metadata.
  const url = new URL(gateway)
  const namespace = `${url.origin}${url.pathname.replace(/\/$/, '')}`
  let closed = false
  let epoch = 0
  let timer: ReturnType<typeof setTimeout> | undefined
  let idle: number | undefined
  let database: Promise<IDBDatabase | undefined> | undefined
  let serial: Promise<void> = Promise.resolve()
  const pending = new Map<string, { writtenAt: number; prepare: () => Stored | undefined }>()
  const open = () => {
    database ??= new Promise<IDBDatabase | undefined>((resolve) => {
      let blocked = false
      const request = indexedDB.open(databaseName, 2)
      request.onupgradeneeded = () => {
        for (const name of [storeName, revocationStore])
          if (!request.result.objectStoreNames.contains(name))
            request.result.createObjectStore(name, { keyPath: ['gateway', 'key'] })
      }
      request.onsuccess = () => {
        const db = request.result
        if (blocked) {
          db.close()
          return
        }
        db.onversionchange = () => db.close()
        resolve(db)
      }
      request.addEventListener('error', () => resolve(undefined))
      request.onblocked = () => {
        blocked = true
        resolve(undefined)
      }
    })
    return database
  }
  const transact = (
    work: (store: IDBObjectStore, revocations: IDBObjectStore) => void,
  ): Promise<void> => {
    const next = serial.then(async () => {
      const db = await open()
      if (db === undefined) return
      await new Promise<void>((resolve, reject) => {
        const tx = db.transaction([storeName, revocationStore], 'readwrite')
        tx.oncomplete = () => resolve()
        tx.addEventListener('error', () => reject(tx.error))
        tx.addEventListener('abort', () => reject(tx.error))
        work(tx.objectStore(storeName), tx.objectStore(revocationStore))
      })
    })
    // Cache I/O is optional: a failure cannot promote, replace or fail an authoritative feed.
    serial = next.catch(() => {})
    return serial
  }
  const recordRevocation = ({
    revocations,
    key,
    at,
  }: {
    readonly revocations: IDBObjectStore
    readonly key: string
    readonly at: number
  }) => {
    revocations.put({ gateway: namespace, key, at })
    const request = revocations.getAll()
    request.onsuccess = () => {
      const decoded = decodeRevocations(request.result)
      if (decoded === undefined) {
        revocations.transaction.abort()
        return
      }
      const rows = decoded.toSorted((a, b) => b.at - a.at)
      let floor = rows.find((row) => row.gateway === '' && row.key === '')?.at ?? -1
      const fences = rows.filter((row) => row.gateway !== '')
      // Compaction keeps a monotone origin-wide write floor, so dropping a tombstone can
      // never make an older queued writer eligible again. It contains no observed data.
      for (const row of fences.slice(reloadCacheLimits.records)) {
        floor = Math.max(floor, row.at)
        revocations.delete([row.gateway, row.key])
      }
      if (floor >= 0) revocations.put({ gateway: '', key: '', at: floor })
    }
  }
  const revoke = (key: string) =>
    transact((store, revocations) => {
      if (key === '*') {
        const request = store.openCursor()
        request.onsuccess = () => {
          const cursor = request.result
          if (cursor === null) return
          if (cursor.value.gateway === namespace) cursor.delete()
          cursor.continue()
        }
      } else store.delete([namespace, key])
      recordRevocation({ revocations, key, at: now() })
    })
  const cancelSchedule = () => {
    clearTimeout(timer)
    if (idle !== undefined) globalThis.cancelIdleCallback?.(idle)
    timer = undefined
    idle = undefined
  }
  const flush = (): Promise<void> => {
    cancelSchedule()
    const batch = [...pending]
    pending.clear()
    if (batch.length === 0) return serial
    return transact((store, revocations) => {
      const prune = () => {
        const retained: Stored[] = []
        const at = now()
        const request = store.openCursor()
        request.onsuccess = () => {
          const cursor = request.result
          if (cursor !== null) {
            const record = validRecord({ raw: cursor.value, now: at })
            if (record === undefined) cursor.delete()
            else retained.push(record)
            cursor.continue()
            return
          }
          retained.sort((a, b) => b.writtenAt - a.writtenAt)
          const counts = new Map<string, number>()
          let total = 0
          let count = 0
          for (const record of retained) {
            const conversations = counts.get(record.gateway) ?? 0
            if (
              count >= reloadCacheLimits.records ||
              total + bytes(record) > reloadCacheLimits.totalBytes ||
              (record.key !== 'agents' && conversations >= reloadCacheLimits.conversations)
            ) {
              store.delete([record.gateway, record.key])
            } else {
              count += 1
              total += bytes(record)
              if (record.key !== 'agents') counts.set(record.gateway, conversations + 1)
            }
          }
        }
      }
      const fences = revocations.getAll()
      fences.onsuccess = () => {
        const denied = decodeRevocations(fences.result)
        if (denied === undefined) {
          store.transaction.abort()
          return
        }
        let remaining = batch.length
        for (const [key, candidate] of batch) {
          const request = store.get([namespace, key])
          request.onsuccess = () => {
            const cutoff = denied.reduce(
              (latest, row) =>
                row.gateway === '' ||
                (row.gateway === namespace && (row.key === key || row.key === '*'))
                  ? Math.max(latest, row.at)
                  : latest,
              -1,
            )
            const previous = validRecord({ raw: request.result, now: now() })
            if (
              candidate.writtenAt > cutoff &&
              (previous === undefined || previous.writtenAt <= candidate.writtenAt) &&
              now() - candidate.writtenAt < reloadCacheLimits.ageMs
            ) {
              let record: Stored | undefined
              try {
                record = candidate.prepare()
              } catch {
                // Unencodable read projections follow the same fence as oversize ones.
              }
              if (record === undefined) {
                store.delete([namespace, key])
                recordRevocation({ revocations, key, at: candidate.writtenAt })
              } else store.put(record)
            }
            remaining -= 1
            if (remaining === 0) prune()
          }
        }
      }
    })
  }
  const schedule = () => {
    if (timer !== undefined || idle !== undefined) return
    timer = globalThis.setTimeout(() => {
      timer = undefined
      if (globalThis.requestIdleCallback !== undefined)
        idle = globalThis.requestIdleCallback(() => void flush(), { timeout: 1000 })
      else void flush()
    }, 300)
  }
  const projection = <A>({
    key,
    decode,
    encode,
  }: {
    readonly key: string
    readonly decode: (raw: unknown) => A | undefined
    readonly encode: (value: A) => string
  }): CachedProjection<A> => ({
    read: async () => {
      const started = epoch
      try {
        const db = await open()
        if (db === undefined || closed || started !== epoch) return undefined
        const raw = await new Promise<unknown>((resolve, reject) => {
          const tx = db.transaction([storeName, revocationStore], 'readonly')
          const request = tx.objectStore(storeName).get([namespace, key])
          const fences = tx.objectStore(revocationStore).getAll()
          tx.oncomplete = () => {
            const record = validRecord({ raw: request.result, now: now() })
            const denied = decodeRevocations(fences.result)
            if (denied === undefined) {
              resolve(undefined)
              return
            }
            const cutoff = denied.reduce(
              (latest, row) =>
                row.gateway === namespace && (row.key === key || row.key === '*')
                  ? Math.max(latest, row.at)
                  : latest,
              -1,
            )
            resolve(record !== undefined && record.writtenAt > cutoff ? record : undefined)
          }
          tx.addEventListener('error', () => reject(tx.error))
          tx.addEventListener('abort', () => reject(tx.error))
        })
        if (closed || started !== epoch) return undefined
        const record = validRecord({ raw, now: now() })
        return record === undefined ? undefined : decode(record.payload)
      } catch {
        return undefined
      }
    },
    write: (value) => {
      if (closed) return
      const writtenAt = now()
      pending.delete(key)
      pending.set(key, {
        writtenAt,
        prepare: () => {
          const payload = encode(value)
          const record: Stored = { version: 1, gateway: namespace, key, writtenAt, payload }
          return bytes(record) <= reloadCacheLimits.recordBytes ? record : undefined
        },
      })
      // Background work is bounded even when many conversations are observed in one turn.
      while (pending.size > reloadCacheLimits.conversations + 1) {
        const oldest = [...pending.keys()].find((candidate) => candidate !== 'agents')
        if (oldest === undefined) break
        pending.delete(oldest)
      }
      schedule()
    },
    remove: () => {
      pending.delete(key)
      return revoke(key)
    },
  })
  return {
    agents: projection({
      key: 'agents',
      decode: (raw) => {
        const rows = Schema.decodeUnknownSync(AgentsJson)(raw)
        return rows.length <= reloadCacheLimits.agents ? rows : undefined
      },
      encode: (rows: readonly Agent[]) => encodeAgents(rows.slice(0, reloadCacheLimits.agents)),
    }),
    conversation: (ref) =>
      projection({
        key: `conversation:${ref}`,
        decode: (raw) => {
          const page = Schema.decodeUnknownSync(PageJson)(raw)
          return page.items.length <= reloadCacheLimits.items ? page : undefined
        },
        encode: (page: ConversationPage) => encodePage(boundedPage(page)),
      }),
    clear: () => {
      epoch += 1
      pending.clear()
      cancelSchedule()
      return revoke('*')
    },
    dispose: async () => {
      closed = true
      await flush()
      const db = await database
      db?.close()
    },
  }
}

/** Only the real browser root opts in; storage denial/private mode leaves the live path unchanged. */
export const browserReloadCache = ({
  gateway,
}: {
  readonly gateway: string
}): ReloadCache | undefined => {
  if (typeof window === 'undefined') return undefined
  try {
    return window.indexedDB === undefined
      ? undefined
      : createReloadCache({ indexedDB: window.indexedDB, gateway })
  } catch {
    return undefined
  }
}
