import { Agent, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'

import { createReloadCache, reloadCacheLimits, type ReloadCache } from './reloadCache.ts'
import type { ConversationPage } from './source.ts'

const caches: ReloadCache[] = []
let clock = 1000
const create = (gateway = 'https://gateway.example') => {
  const cache = createReloadCache({ indexedDB: window.indexedDB, gateway, now: () => clock })
  caches.push(cache)
  return cache
}
const rows = [
  decodeUnknownSync(Agent)({
    id: 'agent/observed',
    kind: 'agent',
    revision: '1',
    updated_at: '2026-10-05T00:00:00Z',
    name: 'Previously observed',
    state: 'running',
    reachability: 'local',
    runtime_ids: [],
  }),
]
const page = (count: number, text = 'Observed transcript'): ConversationPage => ({
  items: Array.from({ length: count }, (_, index) => ({
    _tag: 'Text',
    id: `timeline-entry/${index}`,
    role: 'assistant',
    text,
    attachments: [],
    streaming: false,
    at: '2026-10-05T00:00:00Z',
  })),
  hasOlder: false,
})
const records = async () => {
  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    const request = indexedDB.open('wf.observed@1')
    request.onsuccess = () => resolve(request.result)
    request.addEventListener('error', () => reject(request.error))
  })
  try {
    return await new Promise<Array<{ gateway: string; key: string; payload: string }>>(
      (resolve, reject) => {
        const request = db.transaction('projections').objectStore('projections').getAll()
        request.onsuccess = () => resolve(request.result)
        request.addEventListener('error', () => reject(request.error))
      },
    )
  } finally {
    db.close()
  }
}
beforeEach(async () => {
  clock = 1000
  await new Promise<void>((resolve, reject) => {
    const request = indexedDB.deleteDatabase('wf.observed@1')
    request.onsuccess = () => resolve()
    request.addEventListener('error', () => reject(request.error))
  })
})
afterEach(async () => {
  await Promise.all(caches.splice(0).map((cache) => cache.dispose()))
})

describe('origin-local IndexedDB reload observations', () => {
  it('round trips decoded agents and conversation windows across lifetimes, isolated by gateway', async () => {
    const first = create()
    first.agents.write(rows)
    first.conversation(rows[0]!.id).write(page(1))
    await first.dispose()
    const next = create()
    expect(await next.agents.read()).toEqual(rows)
    expect(await next.conversation(rows[0]!.id).read()).toEqual(page(1))
    expect(await create('https://other-gateway.example').agents.read()).toBeUndefined()
    expect(
      await create('https://gateway.example/other').conversation(rows[0]!.id).read(),
    ).toBeUndefined()
  })

  it('keeps only bounded recent windows and marks truncated history as having older rows', async () => {
    const cache = create()
    for (let index = 0; index < 20; index += 1) {
      clock += 1
      cache.conversation(`agent/${index}`).write(page(300))
    }
    cache.agents.write(rows)
    await cache.dispose()
    const next = create()
    expect(await next.conversation('agent/0').read()).toBeUndefined()
    const newest = await next.conversation('agent/19').read()
    expect(newest).toMatchObject({ hasOlder: true })
    expect(newest?.items.map((item) => item.id)).toEqual(
      Array.from({ length: reloadCacheLimits.items }, (_, index) => `timeline-entry/${index + 44}`),
    )
    expect(await next.agents.read()).toEqual(rows)
    expect(
      (await records()).filter((record) => record.key.startsWith('conversation:')),
    ).toHaveLength(12)
  })

  it('bounds payload bytes across gateways and drops expired or corrupt/version-incompatible entries', async () => {
    for (let index = 0; index < 16; index += 1) {
      const cache = create(`https://gateway-${index}.example`)
      clock += 1
      cache.conversation('agent/large').write(page(2, 'x'.repeat(100_000)))
      await cache.dispose()
    }
    expect(
      await create('https://gateway-0.example').conversation('agent/large').read(),
    ).toBeUndefined()
    expect(await create('https://gateway-15.example').conversation('agent/large').read()).toEqual(
      page(2, 'x'.repeat(100_000)),
    )
    const stored = await records()
    expect(
      stored.reduce((total, record) => total + 2 * record.payload.length, 0),
    ).toBeLessThanOrEqual(reloadCacheLimits.totalBytes)
    expect(
      stored.every((record) => 2 * record.payload.length <= reloadCacheLimits.recordBytes),
    ).toBe(true)
    clock += reloadCacheLimits.ageMs
    expect(
      await create('https://gateway-15.example').conversation('agent/large').read(),
    ).toBeUndefined()

    const cache = create()
    cache.agents.write(rows)
    await cache.dispose()
    const db = await new Promise<IDBDatabase>((resolve) => {
      const request = indexedDB.open('wf.observed@1')
      request.onsuccess = () => resolve(request.result)
    })
    await new Promise<void>((resolve) => {
      const tx = db.transaction('projections', 'readwrite')
      tx.objectStore('projections').put({
        version: 2,
        gateway: 'https://gateway.example',
        key: 'agents',
        writtenAt: clock,
        payload: '[]',
      })
      tx.objectStore('projections').put({
        version: 1,
        gateway: 'https://gateway.example',
        key: 'conversation:agent/broken',
        writtenAt: clock,
        payload: '{"items":[{"_tag":"Text","text":42}],"hasOlder":false}',
      })
      tx.oncomplete = () => resolve()
    })
    db.close()
    const next = create()
    expect(await next.agents.read()).toBeUndefined()
    expect(await next.conversation('agent/broken').read()).toBeUndefined()
  })

  it('removes pending and persisted denied data instead of restoring it on the next reload', async () => {
    const first = create()
    first.agents.write(rows)
    first.conversation('agent/private').write(page(1))
    await first.dispose()
    const revoked = create()
    revoked.agents.write(rows)
    await revoked.agents.remove()
    await revoked.conversation('agent/private').remove()
    await revoked.dispose()
    const next = create()
    expect(await next.agents.read()).toBeUndefined()
    expect(await next.conversation('agent/private').read()).toBeUndefined()
    next.conversation('agent/private').write(page(1))
    await next.clear()
    await next.dispose()
    expect(await create().conversation('agent/private').read()).toBeUndefined()
  })
})

it('fences older queued writers in other tabs even after revocation tombstones are compacted', async () => {
  const delayed = create()
  delayed.agents.write(rows)
  clock += 1
  await create().agents.remove()
  for (let index = 0; index < 40; index += 1) {
    clock += 1
    await create().conversation(`agent/denied-${index}`).remove()
  }
  await delayed.dispose()
  expect(await create().agents.read()).toBeUndefined()
  clock += 1
  const authorized = create()
  authorized.agents.write(rows)
  await authorized.dispose()
  expect(await create().agents.read()).toEqual(rows)
})

it('fences older queued writers after a newer projection cannot be cached', async () => {
  const delayed = create()
  delayed.agents.write(rows)
  delayed.conversation('agent/private').write(page(1))
  clock += 1
  const newer = create()
  newer.agents.write([
    {
      ...rows[0]!,
      name: 'x'.repeat(reloadCacheLimits.recordBytes),
    },
  ])
  const cyclic: { self?: unknown } = {}
  cyclic.self = cyclic
  newer.conversation('agent/private').write({
    items: [
      { _tag: 'UnknownEvent', id: 'timeline-entry/cyclic', eventType: 'unknown', data: cyclic },
    ],
    hasOlder: false,
  })
  await newer.dispose()
  await delayed.dispose()
  const next = create()
  expect(await next.agents.read()).toBeUndefined()
  expect(await next.conversation('agent/private').read()).toBeUndefined()
  clock += 1
  const authorized = create()
  authorized.agents.write(rows)
  await authorized.dispose()
  expect(await create().agents.read()).toEqual(rows)
})
