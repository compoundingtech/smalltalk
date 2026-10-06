import { expect, it } from 'vitest'
import { IDBFactory } from 'fake-indexeddb'
import { createBrowserJournal, decodeStaged } from './journal.ts'
import { migrateFolders } from './run.ts'
import type { LegacySource, MigrationJournal, MigrationOptions } from './run.ts'
import { createTestGateway } from '../testGateway.ts'

class SerialLocks implements LockManager {
  private queue: Promise<void> = Promise.resolve()
  request<TValue>(name: string, callback: LockGrantedCallback<TValue>): Promise<TValue>
  request<TValue>(name: string, options: LockOptions, callback: LockGrantedCallback<TValue>): Promise<TValue>
  request<TValue>(name: string, options: LockOptions | LockGrantedCallback<TValue>, callback?: LockGrantedCallback<TValue>): Promise<TValue> {
    const run = typeof options === 'function' ? options : callback
    if (run === undefined) throw new TypeError('Missing lock callback')
    const result = this.queue.then(() => run({ name, mode: 'exclusive' }))
    this.queue = result.then(() => undefined, () => undefined)
    return result
  }
  query(): Promise<LockManagerSnapshot> { return Promise.resolve({ held: [], pending: [] }) }
}
const sourceFixture = (): { source: LegacySource; state: { frozen: boolean; reads: number; name: string } } => {
  const state = { frozen: false, reads: 0, name: 'Initial' }
  return { state, source: {
    freeze: async () => { state.frozen = true; return { source: 'source/test', token: 'fence/test' } },
    assertFrozen: async (fence) => { if (!state.frozen || fence.token !== 'fence/test') throw new TypeError('legacy writer is active') },
    readFrozen: async () => { state.reads++; return { storeIndex: 5, doc: { folders: { '00000000-0000-7000-8000-000000000002': { name: { value: state.name, at: [5, 0, 'writer'] }, position: { parent: null, key: 'V', at: [5, 0, 'writer'] } } }, placements: {} } } },
  } }
}
const journal = (): MigrationJournal => createBrowserJournal(`migration-${crypto.randomUUID()}`, new IDBFactory(), new SerialLocks())

it('stages before sending, includes edits drained by the fence, then activates exactly once', async () => {
  const gateway = await createTestGateway()
  const legacy = sourceFixture()
  const durable = journal()
  let activated = 0
  legacy.state.name = 'Concurrent pre-fence rename'
  gateway.state.afterAccept = () => { expect(legacy.state.frozen).toBe(true) }
  const options: MigrationOptions = { client: gateway.client, source: legacy.source, journal: durable, name: 'Sidebar', action: { id: 'action/import', idempotencyKey: 'import/test' }, activate: async () => { activated++ } }
  try {
    const result = await migrateFolders(options)
    expect(result.phase).toBe('complete')
    expect((await durable.load())?.phase).toBe('complete')
    expect((await durable.load())?.request.parameters.operations).toContainEqual(expect.objectContaining({ op: 'folder.create', name: 'Concurrent pre-fence rename' }))
    await migrateFolders(options)
    expect(activated).toBe(1)
    expect(legacy.state.reads).toBe(1)
    expect(gateway.calls.filter((call) => call.url === '/v1/client/actions')).toHaveLength(1)
  } finally { await gateway.close() }
})
it('replays exact staged identity after an accepted but lost response, preserving target concurrent edits', async () => {
  const gateway = await createTestGateway()
  const legacy = sourceFixture()
  const durable = journal()
  gateway.state.loseNextReply = true
  gateway.state.afterAccept = () => { gateway.state.current.body.name = { value: 'Later target edit', revision: 'claim/later' }; gateway.state.current.revision = 'claim/later' }
  let activated = 0
  const options: MigrationOptions = { client: gateway.client, source: legacy.source, journal: durable, name: 'Sidebar', action: { id: 'action/import', idempotencyKey: 'import/test' }, activate: async () => { activated++ } }
  try {
    await expect(migrateFolders(options)).rejects.toThrow()
    const staged = await durable.load()
    expect(staged?.phase).toBe('staged')
    expect(activated).toBe(0)
    const resumed = await migrateFolders({ ...options, name: 'Must not regenerate', action: { id: 'action/changed', idempotencyKey: 'changed' } })
    const posts = gateway.calls.filter((call) => call.url === '/v1/client/actions')
    expect(posts).toHaveLength(2)
    expect(posts[1]?.body).toEqual(posts[0]?.body)
    expect(resumed.arrangement.body.name.value).toBe('Later target edit')
    expect(resumed.receipt.arrangement_revision).toBe('claim/import')
    expect(legacy.state.reads).toBe(1)
    expect(activated).toBe(1)
  } finally { await gateway.close() }
})
it('leaves a durable readable stage on cutover failure and resumes without another import', async () => {
  const gateway = await createTestGateway()
  const legacy = sourceFixture()
  const durable = journal()
  let fail = true
  const options: MigrationOptions = { client: gateway.client, source: legacy.source, journal: durable, name: 'Sidebar', action: { id: 'action/import', idempotencyKey: 'import/test' }, activate: async () => { if (fail) throw new TypeError('pointer write refused') } }
  try {
    await expect(migrateFolders(options)).rejects.toThrow('pointer write refused')
    expect((await durable.load())?.phase).toBe('readable')
    fail = false
    await migrateFolders(options)
    expect(gateway.calls.filter((call) => call.url === '/v1/client/actions')).toHaveLength(1)
    expect(legacy.state.frozen).toBe(true)
  } finally { await gateway.close() }
})
it('does not mutate remotely if durable staging fails', async () => {
  const gateway = await createTestGateway()
  const legacy = sourceFixture()
  const durable = journal()
  try {
    await expect(migrateFolders({ client: gateway.client, source: legacy.source, journal: { ...durable, save: async () => { throw new TypeError('disk quota') } }, name: 'Sidebar', action: { id: 'action/import', idempotencyKey: 'import/test' }, activate: async () => { throw new TypeError('must not activate') } })).rejects.toThrow('disk quota')
    expect(gateway.calls.filter((call) => call.url === '/v1/client/actions')).toHaveLength(0)
    expect(legacy.state.frozen).toBe(true)
  } finally { await gateway.close() }
})
it('serializes simultaneous migrators through the real journal interface', async () => {
  const gateway = await createTestGateway()
  const legacy = sourceFixture()
  const durable = journal()
  let activated = 0
  const options: MigrationOptions = { client: gateway.client, source: legacy.source, journal: durable, name: 'Sidebar', action: { id: 'action/import', idempotencyKey: 'import/test' }, activate: async () => { activated++ } }
  try {
    await Promise.all([migrateFolders(options), migrateFolders(options)])
    expect(gateway.calls.filter((call) => call.url === '/v1/client/actions')).toHaveLength(1)
    expect(activated).toBe(1)
  } finally { await gateway.close() }
})
it('rejects corrupt persisted state and unsupported journal versions', () => {
  expect(() => decodeStaged({ version: 2 })).toThrow()
  expect(() => decodeStaged({ version: 1, phase: 'complete' })).toThrow()
})
