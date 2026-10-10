// @vitest-environment jsdom
import { RegistryContext } from '@effect/atom-react'
import type { ActionOf, Arrangement, ArrangementPage, CollectionStreamOptions, EnvelopeOf } from '@smalltalk/st3-client'
import { ActionResult, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import { ActionRefused } from '@st3/sdk/effect'
import { Effect } from 'effect'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import * as React from 'react'
import { flushSync } from 'react-dom'
import { createRoot } from 'react-dom/client'
import { expect, it, vi } from 'vitest'
import type * as Atom from 'effect/reactivity/Atom'
import { sidebarFolders, type FolderState, type SidebarGateway } from './client.ts'
import { optimisticArrangement, type SidebarOperation } from './edit.ts'
import type { SidebarSeat } from './sidebarAdapter.ts'
import { testArrangement, testCapabilities, testSelection, testSnapshot } from './testGateway.ts'
import { useSidebarFolders, type SidebarFoldersBinding } from './useSidebarFolders.ts'

const a = '00000000-0000-7000-8000-000000000002'
const b = '00000000-0000-7000-8000-000000000003'
const initial = () => optimisticArrangement(testArrangement(), [
  { op: 'folder.create', id: a, parent: null, name: 'Inbox', key: 'a0' },
  { op: 'folder.create', id: b, parent: null, name: 'Work', key: 'a1' },
  { op: 'subject.place', subject: 'agent/example', folder: a, key: 'a0' },
])
const seats: readonly SidebarSeat[] = [
  { id: 'alpha.example', subject: 'agent/example', host: 'alpha', label: 'Example' },
  { id: 'beta.example', subject: 'agent/example', host: 'beta', label: 'Example retired' },
]
const refusal = (code: string) => new ActionRefused({ status: 409, message: 'Opaque server detail', response: {
  api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', request_id: 'request/example', code,
  message: `Unknown ${code}: opaque server detail`, retryable: false, details: {},
} })

const mount = async ({ items: startingItems = [initial()], editable = true }: { items?: Arrangement[]; editable?: boolean } = {}) => {
  let items = startingItems
  let streamOptions: CollectionStreamOptions | undefined
  let closes = 0
  let snapshots = 0
  let fail: string | undefined
  let beforeSubmit: (() => void) | undefined
  let gate: Promise<void> | undefined
  const requests: ActionOf<'arrangement.edit'>[] = []
  const envelope = <T,>(value: T): EnvelopeOf<T> => ({ api_version: 'st3.client.v0', request_id: 'request/test', snapshot: testSnapshot, value })
  const gateway: SidebarGateway = {
    discover: async () => envelope({ ...testCapabilities, capabilities: [...testCapabilities.capabilities, { id: 'control.arrangements', state: editable ? 'granted' : 'ungranted', version: 0 }] }),
    arrangementsList: async () => envelope<ArrangementPage>({ kind: 'page', collection: 'arrangements', filters: { person: testSelection.owner }, items, page: { limit: 100, has_more: false, next_cursor: null, cursor_expires_at: null } }),
    collectionStream: async (options) => {
      streamOptions = options
      const noop = () => undefined
      return { subscribeGlasses: noop, subscribeArrangements: noop, subscribe: noop, subscribeTerminal: noop, subscribeConversation: noop, unsubscribe: noop, close: () => { closes++ } }
    },
    actions: {
      snapshot: Effect.sync(() => `snapshot/fresh/${++snapshots}`),
      submitAction: (request) => Effect.gen(function* () {
        requests.push(structuredClone(request))
        beforeSubmit?.()
        if (gate !== undefined) yield* Effect.promise(() => gate!)
        if (fail !== undefined) return yield* Effect.fail(refusal(fail))
        const base = items.find((item) => item.id === request.parameters.subject) ?? { ...testArrangement(), id: request.parameters.subject }
        const operations = request.parameters.operations.filter((operation): operation is SidebarOperation => operation.op.startsWith('folder.') || operation.op === 'subject.place')
        items = [...items.filter((item) => item.id !== base.id), { ...optimisticArrangement(base, operations), revision: `claim/accepted-${requests.length}` }]
        return decodeUnknownSync(ActionResult)({ kind: 'action-result', action_id: request.id, operation_id: 'operation/test', snapshot_id: testSnapshot.id, status: 'completed', affected_ids: [base.id], arrangement_revision: `claim/accepted-${requests.length}` })
      }),
    },
  }
  const source = sidebarFolders({ gateway: () => gateway })
  const registry = AtomRegistry.make()
  let binding: SidebarFoldersBinding | undefined
  let roster = seats
  let query = ''
  const Harness = () => {
    binding = useSidebarFolders({ source, roster, query })
    return <section>{binding.treeStatus}{binding.unavailable}</section>
  }
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  const render = () => flushSync(() => root.render(<RegistryContext.Provider value={registry}><Harness /></RegistryContext.Provider>))
  render()
  await vi.waitFor(() => expect(streamOptions).toBeDefined())
  streamOptions?.onFrame({ kind: 'snapshot', items: [], id: 'arrangements-inventory', collection: 'arrangements', has_more: false, order: [], snapshot: testSnapshot })
  await vi.waitFor(() => expect(registry.get(source).phase).toBe('synced'))
  await React.act(async () => undefined)
  return {
    registry, source, requests, container,
    binding: () => binding!,
    fail: (code?: string) => { fail = code },
    beforeSubmit: (run?: () => void) => { beforeSubmit = run },
    gate: (value?: Promise<void>) => { gate = value },
    items: () => items,
    replace: (next: Arrangement[]) => { items = next },
    updateRoster: (next: readonly SidebarSeat[]) => { roster = next; render() },
    query: (next: string) => { query = next; render() },
    close: () => { flushSync(() => root.unmount()); registry.dispose(); container.remove(); expect(closes).toBe(1) },
  }
}

it('Optimistic move rolls back with fixed refusal reasons on every seat row', async () => {
  const h = await mount()
  const gate = Promise.withResolvers<void>()
  try {
    h.gate(gate.promise)
    h.fail('arrangement-limit')
    flushSync(() => h.binding().onMove({ items: ['alpha.example'], target: { _tag: 'Into', folder: b } }))
    expect(h.binding().status.get('alpha.example')).toEqual({ _tag: 'Pending' })
    expect(h.registry.get(h.source).doc.placements['agent/example']?.folder).toBe(b)
    await vi.waitFor(() => expect(h.requests).toHaveLength(1))
    await React.act(async () => { gate.resolve() })
    await vi.waitFor(() => expect(h.binding().status.get('alpha.example')).toMatchObject({ _tag: 'Refused', reason: 'The folder layout has reached its limit; reduce the layout before trying again.' }))
    expect(h.binding().status.get('beta.example')).toMatchObject({ _tag: 'Refused' })
    expect(h.registry.get(h.source).doc.placements['agent/example']?.folder).toBe(a)
    expect(h.container.textContent).not.toContain('Unknown')
    expect(h.container.textContent).not.toContain('arrangement-limit')
    expect(h.container.textContent).not.toContain('opaque server detail')
  } finally { gate.resolve(); h.close() }
})

it('Concurrent stale fence offers explicit Try again that restages keys and identity against fresh state', async () => {
  const h = await mount()
  try {
    h.fail('stale-fence')
    h.beforeSubmit(() => h.replace([optimisticArrangement(initial(), [
      { op: 'subject.place', subject: 'agent/concurrent', folder: b, key: 'a5' },
    ])]))
    h.updateRoster([...seats, { id: 'alpha.concurrent', subject: 'agent/concurrent', host: 'alpha', label: 'Concurrent' }])
    flushSync(() => h.binding().onMove({ items: ['alpha.example'], target: { _tag: 'Into', folder: b } }))
    await vi.waitFor(() => expect(h.binding().status.get('alpha.example')).toMatchObject({ _tag: 'Refused', onRetry: expect.any(Function) }))
    expect(h.requests).toHaveLength(1)
    const first = h.requests[0]!
    expect(h.registry.get(h.source).doc.placements['agent/example']?.folder).toBe(a)
    h.fail()
    h.beforeSubmit()
    const status = h.binding().status.get('alpha.example')
    if (status?._tag !== 'Refused' || status.onRetry === undefined) throw new TypeError('Expected an explicit fresh-state retry')
    await React.act(async () => status.onRetry?.())
    await vi.waitFor(() => expect(h.registry.get(h.source).phase).toBe('synced'))
    expect(h.requests).toHaveLength(2)
    const retried = h.requests[1]!
    expect(retried.fence.snapshot_id).not.toBe(first.fence.snapshot_id)
    expect(retried.id).not.toBe(first.id)
    expect(retried.idempotency_key).not.toBe(first.idempotency_key)
    const placement = retried.parameters.operations.find((operation) => operation.op === 'subject.place' && operation.subject === 'agent/example')
    expect(placement?.op === 'subject.place' ? placement.key > 'a5' : false).toBe(true)
    expect(h.binding().status.size).toBe(0)
  } finally { h.close() }
})

it('Create with agent sends folders first and keeps the created folder if subsequent filing fails', async () => {
  const h = await mount()
  try {
    h.beforeSubmit(() => { if (h.requests.length === 2) h.fail('forbidden') })
    flushSync(() => h.binding().onCreateFolder({ parent: b, name: 'Together', withAgent: 'agent/example' }))
    await vi.waitFor(() => expect(h.requests).toHaveLength(2))
    await vi.waitFor(() => expect(h.registry.get(h.source).refusal).toBeDefined())
    const creation = h.requests[0]!.parameters.operations.find((operation) => operation.op === 'folder.create')
    expect(creation).toMatchObject({ parent: null, name: 'Together' })
    expect(h.requests[0]!.parameters.operations.some((operation) => operation.op === 'subject.place')).toBe(false)
    expect(h.requests[1]!.parameters.operations).toMatchObject([{ op: 'subject.place', subject: 'agent/example' }])
    expect(creation?.op === 'folder.create' ? h.registry.get(h.source).doc.folders[creation.id]?.name?.value : undefined).toBe('Together')
    expect(h.registry.get(h.source).doc.placements['agent/example']?.folder).toBe(a)
  } finally { h.close() }
})

interface Mounted {
  readonly registry: AtomRegistry.AtomRegistry
  readonly source: Atom.Atom<FolderState>
  readonly requests: readonly ActionOf<'arrangement.edit'>[]
  readonly container: HTMLElement
  readonly binding: () => SidebarFoldersBinding
  readonly fail: (code?: string) => void
  readonly beforeSubmit: (run?: () => void) => void
  readonly replace: (next: Arrangement[]) => void
  readonly updateRoster: (next: readonly SidebarSeat[]) => void
}
/** Refuses the gesture with stale-fence while another writer lands `next`, then presses the explicit Try again. */
const staleThenRetry = async (h: Mounted, next: Arrangement[], gesture: (binding: SidebarFoldersBinding) => void, beforeRetry?: () => void) => {
  h.fail('stale-fence')
  h.beforeSubmit(() => h.replace(next))
  flushSync(() => gesture(h.binding()))
  await vi.waitFor(() => expect(h.registry.get(h.source).retryReady).toBe(true))
  beforeRetry?.()
  await React.act(async () => undefined)
  const first = h.requests.length
  h.fail()
  h.beforeSubmit()
  const retry = h.container.querySelector('button')
  if (retry?.textContent !== 'Try again') throw new TypeError('Expected an explicit fresh-state retry')
  await React.act(async () => { retry.click() })
  await vi.waitFor(() => expect(h.registry.get(h.source).phase).not.toBe('pending'))
  await React.act(async () => undefined)
  return first
}
const legacy = '00000000-0000-7000-8000-000000000008'

it('Try again after a concurrent Sidebar replacement refuses with fixed copy and never writes the new Sidebar', async () => {
  const legacySidebar = { ...initial(), id: `arrangement/${testSelection.owner}/${legacy}` }
  const h = await mount({ items: [legacySidebar] })
  try {
    const sent = await staleThenRetry(h, [legacySidebar, initial()], (binding) => binding.onMove({ items: ['alpha.example'], target: { _tag: 'Into', folder: b } }))
    expect(h.requests).toHaveLength(sent)
    expect(h.requests.every((request) => request.parameters.subject === legacySidebar.id)).toBe(true)
    expect(h.container.textContent).toContain('The folder layout changed elsewhere; make the change again.')
    expect(h.container.querySelector('button')).toBeNull()
    expect(h.items().find((item) => item.id === testSelection.subject)?.body.placements['agent/example']?.value.folder).toBe(a)
  } finally { h.close() }
})

it('Try again with no Sidebar before or after the refusal still bootstraps the reserved identity', async () => {
  const h = await mount({ items: [] })
  try {
    const sent = await staleThenRetry(h, [], (binding) => binding.onCreateFolder({ parent: null, name: 'First' }))
    expect(h.requests).toHaveLength(sent + 1)
    expect(h.requests.at(-1)?.parameters).toMatchObject({ subject: testSelection.subject, operations: [{ op: 'create', name: 'Sidebar' }, { op: 'folder.create', name: 'First' }] })
    expect(h.registry.get(h.source).refusal).toBeUndefined()
  } finally { h.close() }
})

it('Try again after the dragged row key is rebound to another agent refuses instead of moving the replacement', async () => {
  const h = await mount()
  try {
    const replacement = optimisticArrangement(initial(), [{ op: 'subject.place', subject: 'agent/replacement', folder: a, key: 'a1' }])
    h.fail('stale-fence')
    h.beforeSubmit(() => h.replace([replacement]))
    flushSync(() => h.binding().onMove({ items: ['alpha.example'], target: { _tag: 'Into', folder: b } }))
    await vi.waitFor(() => expect(h.registry.get(h.source).retryReady).toBe(true))
    h.updateRoster([{ id: 'alpha.example', subject: 'agent/replacement', host: 'alpha', label: 'Replacement' }])
    await React.act(async () => undefined)
    h.fail()
    h.beforeSubmit()
    await React.act(async () => { h.container.querySelector('button')?.click() })
    await vi.waitFor(() => expect(h.registry.get(h.source).phase).not.toBe('pending'))
    await React.act(async () => undefined)
    expect(h.requests).toHaveLength(1)
    expect(h.items()[0]?.body.placements['agent/replacement']?.value.folder).toBe(a)
    expect(h.container.textContent).toContain('This agent changed; select it again.')
  } finally { h.close() }
})

it('Create with agent refused by stale fence files the agent after Try again creates the folder', async () => {
  const h = await mount()
  try {
    const sent = await staleThenRetry(h, [initial()], (binding) => binding.onCreateFolder({ parent: null, name: 'Together', withAgent: 'alpha.example' }))
    await vi.waitFor(() => expect(h.requests).toHaveLength(sent + 2))
    await vi.waitFor(() => expect(h.registry.get(h.source).phase).toBe('synced'))
    const creation = h.requests[sent]!.parameters.operations.find((operation) => operation.op === 'folder.create')
    expect(creation?.op === 'folder.create' ? h.registry.get(h.source).doc.placements['agent/example']?.folder : undefined).toBe(creation?.id)
    expect(h.requests[sent + 1]!.parameters.operations).toMatchObject([{ op: 'subject.place', subject: 'agent/example' }])
  } finally { h.close() }
})

it('Create with agent shows a fixed refusal and keeps the folder when the seat is rebound before Try again', async () => {
  const h = await mount()
  try {
    const sent = await staleThenRetry(h, [initial()], (binding) => binding.onCreateFolder({ parent: null, name: 'Together', withAgent: 'alpha.example' }),
      () => h.updateRoster([{ id: 'alpha.example', subject: 'agent/replacement', host: 'alpha', label: 'Replacement' }]))
    await vi.waitFor(() => expect(h.registry.get(h.source).phase).toBe('synced'))
    await React.act(async () => undefined)
    expect(h.requests).toHaveLength(sent + 1)
    const creation = h.requests[sent]!.parameters.operations.find((operation) => operation.op === 'folder.create')
    expect(creation?.op === 'folder.create' ? h.registry.get(h.source).doc.folders[creation.id]?.name?.value : undefined).toBe('Together')
    expect(h.registry.get(h.source).doc.placements['agent/example']?.folder).toBe(a)
    expect(h.binding().treeStatus).toBeDefined()
    expect(h.container.textContent).toContain('This agent changed; select it again.')
  } finally { h.close() }
})

it('Try again restages to a fixed refusal for a deleted destination and to Success for an already satisfied move', async () => {
  const deleted = await mount()
  try {
    const sent = await staleThenRetry(deleted, [optimisticArrangement(initial(), [{ op: 'folder.delete', id: b }])], (binding) => binding.onMove({ items: ['alpha.example'], target: { _tag: 'Into', folder: b } }))
    expect(deleted.requests).toHaveLength(sent)
    expect(deleted.registry.get(deleted.source).refusal).toBeDefined()
    expect(deleted.container.textContent).toContain('This folder is no longer available.')
  } finally { deleted.close() }
  const satisfied = await mount()
  try {
    const sent = await staleThenRetry(satisfied, [optimisticArrangement(initial(), [{ op: 'subject.place', subject: 'agent/example', folder: b, key: 'a0' }])], (binding) => binding.onMove({ items: ['alpha.example'], target: { _tag: 'Into', folder: b } }))
    expect(satisfied.requests).toHaveLength(sent)
    expect(satisfied.registry.get(satisfied.source).refusal).toBeUndefined()
    expect(satisfied.registry.get(satisfied.source).phase).toBe('synced')
  } finally { satisfied.close() }
})

it.each([
  { _tag: 'Root', index: 2 } as const,
  { _tag: 'After', sibling: b } as const,
  { _tag: 'Before', sibling: a } as const,
  { _tag: 'Into', folder: b } as const,
])('Filtered rendering keeps canonical ordering for $_tag moves across a hidden sibling', async (target) => {
  const h = await mount()
  try {
    h.query('Example')
    expect(h.binding().tree.map((node) => node.id)).toEqual([a])
    expect(h.binding().orderingTree.map((node) => node.id)).toEqual([a, b])
    // Before exercises a hidden mover; the other targets move the visible folder across a hidden sibling.
    const mover = target._tag === 'Before' ? b : a
    expect(h.binding().canDrop([mover], target)).toEqual({ ok: true })
    flushSync(() => h.binding().onMove({ items: [mover], target }))
    await vi.waitFor(() => expect(h.requests).toHaveLength(1))
    await vi.waitFor(() => expect(h.registry.get(h.source).phase).toBe('synced'))
    await React.act(async () => undefined)
    const canonical = h.binding().orderingTree
    expect(canonical.map((node) => node.id)).toEqual(target._tag === 'Into' ? [b] : [b, a])
    if (target._tag === 'Into') expect(canonical[0]?._tag === 'Folder' ? canonical[0].children.map((node) => node.id) : undefined).toEqual([a])
    expect(h.requests[0]!.parameters.operations).toMatchObject([{ op: 'folder.move', id: mover, parent: target._tag === 'Into' ? b : null }])
  } finally { h.close() }
})

it('Hook rename, delete, collapse and search callbacks work without the kit or domain persistence', async () => {
  const h = await mount()
  try {
    const storage = vi.spyOn(Storage.prototype, 'setItem')
    flushSync(() => h.binding().onToggleCollapsed({ id: a, collapsed: true }))
    expect(h.binding().tree[0]).toMatchObject({ collapsed: true })
    expect(h.requests).toHaveLength(0)
    h.query('Example')
    expect(h.binding().tree).toHaveLength(1)
    flushSync(() => h.binding().onRenameFolder({ id: a, name: 'Renamed' }))
    await vi.waitFor(() => expect(h.registry.get(h.source).doc.folders[a]?.name?.value).toBe('Renamed'))
    await vi.waitFor(() => expect(h.registry.get(h.source).phase).toBe('synced'))
    flushSync(() => h.binding().onDeleteFolder({ id: a }))
    await vi.waitFor(() => expect(h.registry.get(h.source).doc.folders[a]?.deleted).toBeDefined())
    await vi.waitFor(() => expect(h.registry.get(h.source).phase).toBe('synced'))
    expect(h.registry.get(h.source).doc.placements['agent/example']?.folder).toBe(a)
    expect(h.binding().tree.map((node) => node.id)).toEqual(['host:alpha', 'host:beta'])
    flushSync(() => h.binding().onToggleCollapsed({ id: 'host:alpha', collapsed: true }))
    expect(h.binding().tree[0]).toMatchObject({ collapsed: true })
    expect(storage).not.toHaveBeenCalled()
    storage.mockRestore()
  } finally { h.close() }
})

it('No live Sidebar bootstraps the reserved identity only on a user edit', async () => {
  const h = await mount({ items: [] })
  try {
    expect(h.requests).toEqual([])
    flushSync(() => h.binding().onCreateFolder({ parent: null, name: 'First' }))
    await vi.waitFor(() => expect(h.requests).toHaveLength(1))
    await vi.waitFor(() => expect(h.registry.get(h.source).phase).toBe('synced'))
    expect(h.requests[0]?.parameters.subject).toBe(testSelection.subject)
    expect(h.requests[0]?.parameters.operations[0]).toEqual({ op: 'create', name: 'Sidebar' })
  } finally { h.close() }
})

it('Legacy Sidebar selection reports multiples without folding or retiring resources', async () => {
  const later = { ...initial(), id: 'arrangement/person/example/00000000-0000-7000-8000-000000000009' }
  const earlier = { ...initial(), id: 'arrangement/person/example/00000000-0000-7000-8000-000000000008' }
  const h = await mount({ items: [later, earlier] })
  try {
    expect(h.registry.get(h.source).sidebarSubject).toBe(earlier.id)
    expect(h.container.textContent).toContain('Several sidebar layouts are available.')
    expect(h.requests).toEqual([])
    expect(h.items()).toHaveLength(2)
    expect(h.container.textContent).not.toContain(earlier.id)
  } finally { h.close() }
})

it('Renamed reserved Sidebar preserves identity; retired reserved Sidebar shows Restore unavailable', async () => {
  const renamed = initial()
  renamed.body.name.value = 'Renamed layout'
  const h = await mount({ items: [renamed] })
  try { expect(h.registry.get(h.source).sidebarSubject).toBe(testSelection.subject) } finally { h.close() }
  const retired = await mount()
  retired.replace([])
  retired.registry.get(retired.source).retry?.()
  await vi.waitFor(() => expect(retired.registry.get(retired.source).restoreUnavailable).toBe(true))
  await React.act(async () => undefined)
  try {
    expect(retired.binding().unavailable).toBe('Restore unavailable.')
    expect(retired.binding().canDrop([a], { _tag: 'Root', index: 0 })).toHaveProperty('refused')
    flushSync(() => retired.binding().onCreateFolder({ parent: null, name: 'Never recreated' }))
    expect(retired.requests).toEqual([])
  } finally { retired.close() }
})

it('Read-only sidebar refuses drops and writes with fixed unavailable copy', async () => {
  const h = await mount({ editable: false })
  try {
    expect(h.binding().canDrop(['alpha.example'], { _tag: 'Into', folder: b })).toEqual({ refused: 'Folder editing is unavailable for this connection.' })
    flushSync(() => h.binding().onDeleteFolder({ id: a }))
    expect(h.requests).toEqual([])
    expect(h.container.textContent).toBe('Folder editing is unavailable for this connection.')
  } finally { h.close() }
})
