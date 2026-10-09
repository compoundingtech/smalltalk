import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import type { Arrangement, ArrangementPage, CollectionStream, CollectionStreamOptions, EnvelopeOf } from '@smalltalk/st3-client'
import { Effect } from 'effect'
import { ActionResult, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import { optimisticArrangement, type SidebarOperation } from './edit.ts'
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
    expect(await registry.get(folders).edit([])).toMatchObject({ _tag: 'Refused', reason: { _tag: 'Unknown' } })
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
    actions: { snapshot: Effect.succeed(testSnapshot.id), submitAction: () => Effect.die('A read-only pairing must not submit arrangement actions.') },
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
    expect(registry.get(atom).sidebarSubject).toBe(earlier.id)
    expect(registry.get(atom).sidebarCandidates?.map((candidate) => candidate.id)).toEqual([earlier.id, later.id])
    expect(registry.get(atom).sidebarCandidates?.every((candidate) => candidate.label.includes(candidate.id))).toBe(true)
  } finally { unsubscribe(); registry.dispose() }
})

it('binds a granted live Sidebar atom to optimistic generated edits and exactly one creation', async () => {
  let items: Arrangement[] = []
  let options: CollectionStreamOptions | undefined
  const submitted: SidebarOperation[][] = []
  const folder = '00000000-0000-7000-8000-000000000002'
  const envelope = <T>(value: T): EnvelopeOf<T> => ({ api_version: 'st3.client.v0', request_id: 'request/test', snapshot: testSnapshot, value })
  const gateway: SidebarGateway = {
    discover: async () => envelope({ ...testCapabilities, capabilities: [...testCapabilities.capabilities, { id: 'control.arrangements', version: 0, state: 'granted' }] }),
    arrangementsList: async () => envelope<ArrangementPage>({ kind: 'page', collection: 'arrangements', filters: { person: 'person/example' }, items, page: { limit: 100, has_more: false, next_cursor: null, cursor_expires_at: null } }),
    collectionStream: async (next) => {
      options = next
      const noop = () => undefined
      return { subscribeGlasses: noop, subscribeArrangements: noop, subscribe: noop, subscribeTerminal: noop, subscribeConversation: noop, unsubscribe: noop, close: noop }
    },
    actions: {
      snapshot: Effect.succeed(testSnapshot.id),
      submitAction: (request) => Effect.sync(() => {
        const operations = request.parameters.operations.filter((operation): operation is SidebarOperation =>
          operation.op.startsWith('folder.') || operation.op === 'subject.place')
        submitted.push(operations)
        items = [optimisticArrangement(items[0] ?? { ...testArrangement(), id: request.parameters.subject }, operations)]
        return decodeUnknownSync(ActionResult)({ kind: 'action-result', action_id: request.id, operation_id: 'operation/edit', snapshot_id: testSnapshot.id, status: 'completed', affected_ids: [request.parameters.subject], arrangement_revision: 'claim/edit' })
      }),
    },
  }
  const atom = sidebarFolders({ gateway: () => gateway })
  const registry = AtomRegistry.make()
  const unsubscribe = registry.subscribe(atom, () => {})
  try {
    expect(registry.get(atom).phase).toBe('connecting')
    await vi.waitFor(() => expect(options).toBeDefined())
    options?.onFrame({ kind: 'snapshot', items: [], id: 'arrangements-inventory', collection: 'arrangements', has_more: false, order: [], snapshot: testSnapshot })
    await vi.waitFor(() => expect(registry.get(atom).phase).toBe('synced'))
    expect(registry.get(atom).readOnly).toBe(false)
    const pending = registry.get(atom).edit([{ op: 'folder.create', id: folder, name: 'Inbox', parent: null, key: 'a0' }])
    expect(project(registry.get(atom).doc, []).folders[0]?.name).toBe('Inbox')
    expect(registry.get(atom).phase).toBe('pending')
    expect(await pending).toEqual({ _tag: 'Success' })
    expect(items).toHaveLength(1)
    expect(submitted).toHaveLength(1)
    expect(registry.get(atom).phase).toBe('synced')
  } finally { unsubscribe(); registry.dispose() }
})

it.each([false, true])('propagates adoption metadata and a removed reserved Sidebar without mutations (granted=%s)', async (granted) => {
  const earlier = { ...testArrangement(), id: 'arrangement/person/example/00000000-0000-7000-8000-000000000003' }
  const later = { ...testArrangement(), id: 'arrangement/person/example/00000000-0000-7000-8000-000000000009' }
  const reserved = testArrangement()
  reserved.body.name.value = 'Renamed layout'
  let items: Arrangement[] = [later, earlier]
  let options: CollectionStreamOptions | undefined
  let submissions = 0
  let subscriptions = 0
  const envelope = <T>(value: T): EnvelopeOf<T> => ({ api_version: 'st3.client.v0', request_id: 'request/test', snapshot: testSnapshot, value })
  const gateway: SidebarGateway = {
    discover: async () => envelope({
      ...testCapabilities,
      capabilities: [...testCapabilities.capabilities, ...(granted ? [{ id: 'control.arrangements', version: 0, state: 'granted' as const }] : [])],
    }),
    arrangementsList: async () => envelope<ArrangementPage>({
      kind: 'page', collection: 'arrangements', filters: { person: 'person/example' }, items,
      page: { limit: 100, has_more: false, next_cursor: null, cursor_expires_at: null },
    }),
    collectionStream: async (next) => {
      options = next
      subscriptions++
      const noop = () => undefined
      return { subscribeGlasses: noop, subscribeArrangements: noop, subscribe: noop, subscribeTerminal: noop, subscribeConversation: noop, unsubscribe: noop, close: noop }
    },
    actions: {
      snapshot: Effect.succeed(testSnapshot.id),
      submitAction: () => Effect.sync(() => { submissions++ }).pipe(Effect.flatMap(() => Effect.die('This metadata-only test must not submit actions.'))),
    },
  }
  const atom = sidebarFolders({ gateway: () => gateway })
  const registry = AtomRegistry.make()
  const unsubscribe = registry.subscribe(atom, () => {})
  const frame = (kind: 'snapshot' | 'changes') => options?.onFrame({
    ...(kind === 'snapshot' ? { kind, items: [] } : { kind, upserts: [], removes: [] }),
    id: 'arrangements-inventory', collection: 'arrangements', has_more: false, order: [], snapshot: testSnapshot,
  } as Parameters<CollectionStreamOptions['onFrame']>[0])
  try {
    registry.get(atom)
    await vi.waitFor(() => expect(options).toBeDefined())
    frame('snapshot')
    await vi.waitFor(() => expect(registry.get(atom).sidebarSubject).toBe(earlier.id))
    expect(registry.get(atom).sidebarCandidates?.map((candidate) => candidate.id)).toEqual([earlier.id, later.id])
    expect(registry.get(atom).sidebarCandidates?.every((candidate) => candidate.label.includes(candidate.id))).toBe(true)
    items = []
    frame('changes')
    await vi.waitFor(() => expect(registry.get(atom).sidebarSubject).toBeUndefined())
    expect(registry.get(atom).restoreUnavailable).not.toBe(true)
    expect(registry.get(atom).sidebarCandidates).toBeUndefined()
    items = [later, reserved, earlier]
    frame('changes')
    await vi.waitFor(() => expect(registry.get(atom).sidebarSubject).toBe(reserved.id))
    expect(registry.get(atom).restoreUnavailable).not.toBe(true)
    items = []
    frame('changes')
    await vi.waitFor(() => expect(registry.get(atom).restoreUnavailable).toBe(true))
    expect(registry.get(atom)).toMatchObject({ phase: 'synced', readOnly: true, sidebarSubject: reserved.id, doc: { folders: {}, placements: {} } })
    const outcome = await registry.get(atom).edit([{ op: 'folder.create', id: '00000000-0000-7000-8000-000000000002', name: 'Inbox', parent: null, key: 'a0' }])
    expect(outcome).toMatchObject({ _tag: 'Refused', detail: 'Restoring a removed Sidebar is not available yet.' })
    items = [reserved]
    frame('changes')
    await vi.waitFor(() => expect(registry.get(atom).restoreUnavailable).not.toBe(true))
    expect(registry.get(atom).sidebarSubject).toBe(reserved.id)
    expect(submissions).toBe(0)
    expect(subscriptions).toBe(1)
  } finally { unsubscribe(); registry.dispose() }
})
