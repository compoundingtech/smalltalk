import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import type { Arrangement, ArrangementPage, CollectionStream, CollectionStreamOptions, EnvelopeOf } from '@smalltalk/st3-client'
import { expect, it, vi } from 'vitest'
import { arrangementSidebarDoc, folders, sidebarFolders, type SidebarGateway } from './client.ts'
import { testArrangement, testCapabilities, testSnapshot } from './testGateway.ts'
import { project } from './core.mts'

it('fails unavailable before listing or editing when arrangements are ungranted', async () => {
  const requests: string[] = []
  vi.stubGlobal('location', { origin: 'http://sidebar.test' })
  vi.stubGlobal('fetch', async (input: RequestInfo | URL) => {
    requests.push(String(input))
    return Response.json({ api_version: 'st3.client.v0', request_id: 'request/test', snapshot: testSnapshot,
      value: { ...testCapabilities, capabilities: [{ id: 'arrangements', state: 'ungranted', version: 1 }] } })
  })
  const registry = AtomRegistry.make()
  const unsubscribe = registry.subscribe(folders, () => {})
  try {
    await vi.waitFor(() => expect(registry.get(folders).phase).toBe('unavailable'))
    expect(registry.get(folders).detail).toContain('Granted arrangements capability version 1')
    expect(registry.get(folders).readOnly).toBe(true)
    expect(requests).toEqual(['http://sidebar.test/v1/client/capabilities'])
    expect(project(registry.get(folders).doc, ['agent/a']).unfiled).toEqual(['agent/a'])
    expect(() => registry.get(folders).edit([])).toThrow('read-only')
  } finally { unsubscribe(); registry.dispose(); vi.unstubAllGlobals() }
})

it('projects authoritative register values and tombstones without editing an arrangement', () => {
  const arrangement = testArrangement()
  const id = '00000000-0000-7000-8000-000000000002'
  arrangement.body.folders[id] = {
    name: { value: 'Inbox', revision: 'claim/name' },
    position: { value: { parent: null, key: 'a0' }, revision: 'claim/position' },
    tombstone: null,
  }
  arrangement.body.placements['agent/a'] = { value: { folder: id, key: 'a0' }, revision: 'claim/place' }
  const original = JSON.stringify(arrangement)
  expect(project(arrangementSidebarDoc(arrangement), ['agent/a']).folders[0]).toMatchObject({ id, name: 'Inbox', members: ['agent/a'] })
  expect(JSON.stringify(arrangement)).toBe(original)
})

it('shows the lowest-UUIDv7 arrangement and switches when an empty owner frame reveals a lower one', async () => {
  const sidebar = (uuid: string, folder: string): Arrangement => {
    const value = testArrangement()
    value.id = `arrangement/person/example/${uuid}`
    value.body.folders['00000000-0000-7000-8000-0000000000f0'] = {
      name: { value: folder, revision: 'claim/name' },
      position: { value: { parent: null, key: 'a0' }, revision: 'claim/position' },
      tombstone: null,
    }
    return value
  }
  const later = sidebar('00000000-0000-7000-8000-000000000009', 'Later')
  const earlier = sidebar('00000000-0000-7000-8000-000000000003', 'Earlier')
  let items = [later]
  let reads = 0
  let options: CollectionStreamOptions | undefined
  const envelope = <A>(value: A) => ({ api_version: 'st3.client.v0', request_id: 'request/test', snapshot: testSnapshot, value }) as EnvelopeOf<A>
  const gateway: SidebarGateway = {
    discover: async () => envelope({ ...testCapabilities, session_actor: 'person/example/session/one' }),
    arrangementsList: async () => {
      reads++
      return envelope<ArrangementPage>({ kind: 'page', collection: 'arrangements', filters: { person: 'person/example' }, items, page: { limit: 100, has_more: false, next_cursor: null, cursor_expires_at: null } })
    },
    collectionStream: async (next) => {
      options = next
      const noop = () => undefined
      return { subscribeGlasses: noop, subscribeArrangements: noop, subscribe: noop, subscribeTerminal: noop, subscribeConversation: noop, unsubscribe: noop, close: noop } satisfies CollectionStream
    },
  }
  const atom = sidebarFolders({ gateway: () => gateway })
  const registry = AtomRegistry.make()
  let updates = 0
  const unsubscribe = registry.subscribe(atom, () => { updates++ })
  const frame = (kind: 'snapshot' | 'changes') => options?.onFrame({ ...(kind === 'snapshot' ? { kind, items: [] } : { kind, upserts: [], removes: [] }), id: 'arrangements-inventory', collection: 'arrangements', has_more: true, order: [], snapshot: testSnapshot } as Parameters<CollectionStreamOptions['onFrame']>[0])
  const folderNames = () => project(registry.get(atom).doc, []).folders.map((folder) => folder.name)
  try {
    expect(registry.get(atom).phase).toBe('connecting')
    await vi.waitFor(() => expect(options).toBeDefined())
    frame('snapshot')
    await vi.waitFor(() => expect(registry.get(atom).phase).toBe('synced'))
    expect(folderNames()).toEqual(['Later'])
    const published = updates
    // An unchanged reread publishes nothing; the lower UUIDv7 then replaces the winner once.
    frame('changes')
    items = [later, earlier]
    frame('changes')
    await vi.waitFor(() => expect(folderNames()).toEqual(['Earlier']))
    expect(reads).toBe(3)
    expect(updates).toBe(published + 1)
    expect(registry.get(atom)).toMatchObject({ phase: 'synced', readOnly: true })
  } finally { unsubscribe(); registry.dispose() }
})
