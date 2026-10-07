import { Schema } from 'effect'
import { ArrangementEditParameters } from '@smalltalk/st3-client/schema'
import type { MigrationJournal, StagedMigration } from './run.ts'
import { decodeDoc } from './legacy.ts'

const parameters = Schema.toEncoded(ArrangementEditParameters)
const stagedSchema = Schema.Struct({
  version: Schema.Literal(1), phase: Schema.Literals(['staged', 'readable', 'complete']),
  fence: Schema.Struct({ source: Schema.String, token: Schema.String }),
  source: Schema.Struct({ doc: Schema.Unknown, storeIndex: Schema.Number.check(Schema.makeFilter((index) => Number.isSafeInteger(index) && index >= 0)) }),
  request: Schema.Struct({ id: Schema.String, idempotency_key: Schema.String,
    fence: Schema.Struct({ snapshot_id: Schema.String, subject_revisions: Schema.Record(Schema.String, Schema.String) }),
    parameters,
  }),
  receipt: Schema.optionalKey(Schema.Struct({ action_id: Schema.String, affected_ids: Schema.Array(Schema.String),
    arrangement_revision: Schema.optionalKey(Schema.String), kind: Schema.Literal('action-result'), operation_id: Schema.String,
    snapshot_id: Schema.String, status: Schema.Literals(['accepted', 'completed', 'rejected']),
  })),
})
export const decodeStaged = (value: unknown): StagedMigration => {
  const { receipt, ...parsed } = Schema.decodeUnknownSync(stagedSchema)(value)
  if (parsed.phase !== 'staged' && receipt === undefined) throw new TypeError('Readable/complete migrations require an admission receipt')
  return { ...parsed,
    source: { ...parsed.source, doc: decodeDoc(parsed.source.doc) },
    request: { ...parsed.request, parameters: { ...parsed.request.parameters, operations: [...parsed.request.parameters.operations] } },
    ...(receipt === undefined ? {} : { receipt: { ...receipt, affected_ids: [...receipt.affected_ids] } }),
  }
}

/** One database per source migration; callers explicitly choose the durable namespace.
 * Web Locks serialize browser tabs; strict IDB durability stages before remote admission. */
export const createBrowserJournal = (name: string, factory: IDBFactory = indexedDB, locks: LockManager = navigator.locks): MigrationJournal => {
  if (!name || locks === undefined) throw new TypeError('A journal namespace and Web Locks are required')
  const open = (): Promise<IDBDatabase> => new Promise((resolve, reject) => {
    const request = factory.open(name, 1)
    request.onupgradeneeded = () => request.result.createObjectStore('migration')
    request.onerror = () => reject(request.error)
    request.onblocked = () => reject(new TypeError('Migration journal upgrade is blocked by another tab'))
    request.onsuccess = () => resolve(request.result)
  })
  const access = async (mode: IDBTransactionMode, value?: StagedMigration): Promise<StagedMigration | undefined> => {
    const database = await open()
    try {
      return await new Promise<StagedMigration | undefined>((resolve, reject) => {
        const transaction = database.transaction('migration', mode, { durability: 'strict' })
        const store = transaction.objectStore('migration')
        const request = value === undefined ? store.get('state') : store.put(value, 'state')
        let result: StagedMigration | undefined
        request.onsuccess = () => {
          if (value !== undefined || request.result === undefined) return
          try { result = decodeStaged(request.result) }
          catch (cause) { transaction.abort(); reject(cause) }
        }
        transaction.oncomplete = () => resolve(result)
        transaction.onerror = () => reject(transaction.error)
        transaction.onabort = () => reject(transaction.error ?? new TypeError('Migration journal transaction aborted'))
      })
    } finally { database.close() }
  }
  return {
    exclusive: (run) => locks.request(`fractal-folders:${name}`, { mode: 'exclusive' }, run),
    load: () => access('readonly'),
    save: async (value) => { await access('readwrite', decodeStaged(value)) },
  }
}
