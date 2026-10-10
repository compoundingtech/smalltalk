import { describe, expect, it } from '@effect/vitest'
import { ArrangementEditParameters } from '@smalltalk/st3-client/schema'
import { Effect, Schema } from 'effect'
import * as fc from 'fast-check'
import { arrangementSidebarDoc } from './client.ts'
import { drawnParents, liveAncestor, type FolderDoc } from './core.mts'
import { optimisticArrangement, type SidebarOperation } from './edit.ts'
import { boundRows, canDrop, filterSidebarTree, nativeInsertAt, operationsForCreate, operationsForDelete, operationsForMove, operationsForRename, restageBoundMove, restageCreate, restageDelete, restageMove, restageRename, rowStatus, sidebarProjection, sidebarTree, type SidebarDocument, type SidebarSeat } from './sidebarAdapter.ts'
import type { SidebarMove } from './sidebarContract.ts'
import { testArrangement } from './testGateway.ts'

const a = '00000000-0000-7000-8000-000000000002'
const b = '00000000-0000-7000-8000-000000000003'
const child = '00000000-0000-7000-8000-000000000004'
const fresh = '00000000-0000-7000-8000-000000000005'
const roster: readonly SidebarSeat[] = [
  { id: 'beta.one', subject: 'agent/one', host: 'beta', label: 'One' },
  { id: 'alpha.one', subject: 'agent/one', host: 'alpha', label: 'One retired' },
  { id: 'alpha.two', subject: 'agent/two', host: 'alpha', label: 'Two' },
  { id: 'alpha.new', subject: 'agent/new', host: 'alpha', label: 'New' },
]
const setup = (): SidebarDocument => ({ ...arrangementSidebarDoc(optimisticArrangement(testArrangement(), [
  { op: 'folder.create', id: a, parent: null, name: 'A', key: 'a0' },
  { op: 'folder.create', id: b, parent: null, name: 'B', key: 'a1' },
  { op: 'folder.create', id: child, parent: a, name: 'Child', key: 'a0' },
  { op: 'subject.place', subject: 'agent/one', folder: a, key: 'a0' },
  { op: 'subject.place', subject: 'agent/two', folder: a, key: 'a1' },
  { op: 'subject.place', subject: 'agent/dormant', folder: a, key: 'a2' },
])), roster })
const apply = (doc: SidebarDocument, operations: readonly SidebarOperation[]): SidebarDocument => {
  const arrangement = testArrangement()
  // Exercise generated operations through the production optimistic reducer, not a second test reducer.
  arrangement.body.folders = Object.fromEntries(Object.entries(doc.folders).map(([id, regs]) => [id, {
    name: { value: regs.name?.value ?? '', revision: regs.name?.at[2] ?? 'claim/test' },
    position: { value: { parent: regs.position?.parent ?? null, key: regs.position?.key ?? '' }, revision: regs.position?.at[2] ?? 'claim/test' },
    tombstone: regs.deleted === undefined ? null : { value: true, revision: regs.deleted[2] },
  }]))
  arrangement.body.placements = Object.fromEntries(Object.entries(doc.placements).map(([subject, value]) => [subject, { value: { folder: value.folder, key: value.key }, revision: value.at[2] }]))
  return { ...arrangementSidebarDoc(optimisticArrangement(arrangement, operations)), roster: doc.roster }
}
const move = (id: string, target: SidebarMove['target']): SidebarMove => ({ items: [id], target })
const admissible = (operations: readonly SidebarOperation[]) =>
  Schema.is(Schema.toEncoded(ArrangementEditParameters))({ owner: 'person/example', subject: 'arrangement/person/example/00000000-0000-7000-8000-000000000001', operations })

describe('sidebar matrix operations', () => {
  it('Create folder appends, normalizes blank names and falls back from a missing parent', () => {
    const doc = setup()
    const created = operationsForCreate(doc, { parent: a, name: '  ', target: { _tag: 'Root', index: 0 } }, fresh)
    expect(created).toMatchObject([{ op: 'folder.create', id: fresh, parent: a, name: 'New folder' }])
    expect(apply(doc, created).folders[fresh]!.position!.key > doc.folders[child]!.position!.key).toBe(true)
    expect(operationsForCreate(doc, { parent: 'missing', name: ' New ' }, fresh)).toMatchObject([{ parent: null, name: 'New' }])
  })
  it('Create folder with agent plans root creation before subject filing', () => {
    expect(operationsForCreate(setup(), { parent: a, name: 'Together', withAgent: 'agent/one' }, fresh)).toMatchObject([
      { op: 'folder.create', id: fresh, parent: null }, { op: 'subject.place', subject: 'agent/one', folder: fresh },
    ])
  })
  it('Rename folder preserves identity and registers, with same-name and gone-target no-ops', () => {
    const doc = setup()
    expect(operationsForRename(doc, { id: a, name: ' A ' })).toEqual([])
    expect(operationsForRename(doc, { id: 'missing', name: 'A' })).toEqual([])
    const next = apply(doc, operationsForRename(doc, { id: a, name: '  ' }))
    expect(next.folders[a]?.name?.value).toBe('New folder')
    expect(next.folders[a]?.position).toEqual(doc.folders[a]?.position)
    expect(next.placements).toEqual(doc.placements)
  })
  it('Delete folder lifts children and members retaining keys, without deleting agents', () => {
    const doc = setup()
    const operations = operationsForDelete(doc, { id: a })
    expect(operations).toEqual([{ op: 'folder.delete', id: a }])
    const next = apply(doc, operations)
    expect(next.placements).toEqual(doc.placements)
    expect(next.folders[child]?.position).toEqual(doc.folders[child]?.position)
    expect(sidebarProjection(next).folders.map((folder) => folder.id)).toEqual([child, b])
    expect(sidebarProjection(next).unfiled).toContain('agent/one')
    expect(operationsForDelete(next, { id: a })).toEqual([])
    const nested = apply(doc, [{ op: 'folder.move', id: a, parent: b, key: 'a0' }])
    expect(sidebarProjection(apply(nested, operationsForDelete(nested, { id: a }))).folders[0]?.members).toEqual(['agent/one', 'agent/two'])
  })
  it('File agent into folder appends one shared membership for every seat', () => {
    const doc = setup()
    const operations = operationsForMove(doc, move('alpha.new', { _tag: 'Into', folder: a }))
    expect(operations).toMatchObject([{ op: 'subject.place', subject: 'agent/new', folder: a }])
    expect(sidebarProjection(apply(doc, operations)).folders[0]?.members).toEqual(['agent/one', 'agent/two', 'agent/new'])
    expect(operationsForMove(doc, move('alpha.one', { _tag: 'Into', folder: b }))).toHaveLength(1)
  })
  it('Unfile agent writes null and a0, then unfiled-to-unfiled is silent', () => {
    const doc = setup()
    const operations = operationsForMove(doc, move('beta.one', { _tag: 'Unfiled' }))
    expect(operations).toEqual([{ op: 'subject.place', subject: 'agent/one', folder: null, key: 'a0' }])
    expect(operationsForMove(apply(doc, operations), move('alpha.one', { _tag: 'Unfiled' }))).toEqual([])
  })
  it('Reorder agents within folder uses subject slots, not individual seats or dormant slots', () => {
    const doc = setup()
    const next = apply(doc, operationsForMove(doc, move('alpha.two', { _tag: 'Before', sibling: 'beta.one' })))
    expect(sidebarProjection(next).folders[0]?.members).toEqual(['agent/two', 'agent/one'])
    expect(next.placements['agent/dormant']).toEqual(doc.placements['agent/dormant'])
  })
  it('Reorder agents at top level is refused, including manual Root indices', () => {
    const doc = setup()
    expect(canDrop(doc, move('alpha.new', { _tag: 'Root', index: 0 }))).toEqual({ refused: 'Unfiled agents use automatic host order.' })
    expect(canDrop(doc, move('alpha.new', { _tag: 'After', sibling: 'alpha.new' }))).toHaveProperty('refused')
  })
  it('Reorder folders respects the sibling folder partition and silent no-ops', () => {
    const doc = setup()
    const next = apply(doc, operationsForMove(doc, move(b, { _tag: 'Before', sibling: a })))
    expect(sidebarProjection(next).folders.map((folder) => folder.id)).toEqual([b, a])
    expect(operationsForMove(next, move(b, { _tag: 'Before', sibling: a }))).toEqual([])
    expect(operationsForMove(doc, move(a, { _tag: 'Before', sibling: a }))).toEqual([])
  })
  it('Nest folder appends its subtree and refuses self or descendant cycles', () => {
    const doc = setup()
    const next = apply(doc, operationsForMove(doc, move(b, { _tag: 'Into', folder: a })))
    expect(sidebarProjection(next).folders[0]?.folders.map((folder) => folder.id)).toEqual([child, b])
    for (const folder of [a, child]) expect(canDrop(doc, move(a, { _tag: 'Into', folder }))).toEqual({ refused: 'A folder cannot go inside itself or its subfolders.' })
    expect(operationsForMove(doc, move(a, { _tag: 'Into', folder: child }))).toEqual([])
  })
  it('Move folder back to top level indexes only root folders', () => {
    const doc = setup()
    const next = apply(doc, operationsForMove(doc, move(child, { _tag: 'Root', index: 1 })))
    expect(sidebarProjection(next).folders.map((folder) => folder.id)).toEqual([a, child, b])
    expect(canDrop(doc, move(a, { _tag: 'Root', index: 20 }))).toHaveProperty('refused')
  })
  it('Root gap indices precede mover removal, including later gaps and the final gap', () => {
    const doc = apply(setup(), operationsForCreate(setup(), { parent: null, name: 'Last' }, fresh))
    expect(sidebarProjection(doc).folders.map((folder) => folder.id)).toEqual([a, b, fresh])
    expect(sidebarProjection(apply(doc, operationsForMove(doc, move(a, { _tag: 'Root', index: 2 })))).folders.map((folder) => folder.id)).toEqual([b, a, fresh])
    expect(canDrop(doc, move(a, { _tag: 'Root', index: 3 }))).toEqual({ ok: true })
    expect(sidebarProjection(apply(doc, operationsForMove(doc, move(a, { _tag: 'Root', index: 3 })))).folders.map((folder) => folder.id)).toEqual([b, fresh, a])
    expect(operationsForMove(doc, move(a, { _tag: 'Root', index: 1 }))).toEqual([])
    expect(operationsForMove(doc, move(b, { _tag: 'Root', index: 1 }))).toEqual([])
    expect(sidebarProjection(apply(doc, operationsForMove(doc, move(fresh, { _tag: 'Root', index: 0 })))).folders.map((folder) => folder.id)).toEqual([fresh, a, b])
  })
  it('Into appends to the mover partition without changing the other partition', () => {
    const doc = setup()
    const folders = apply(doc, operationsForMove(doc, move(b, { _tag: 'Into', folder: a })))
    expect(sidebarProjection(folders).folders[0]?.folders.map((folder) => folder.id)).toEqual([child, b])
    expect(folders.placements).toEqual(doc.placements)
    const members = apply(doc, operationsForMove(doc, move('alpha.new', { _tag: 'Into', folder: a })))
    expect(sidebarProjection(members).folders[0]?.members).toEqual(['agent/one', 'agent/two', 'agent/new'])
    expect(members.folders).toEqual(doc.folders)
  })
  it('A one-row move resolves its shared subject from the tree rather than accepting a subject as a row id', () => {
    const doc = setup()
    expect(operationsForMove(doc, move('beta.one', { _tag: 'Into', folder: b }))).toMatchObject([{ op: 'subject.place', subject: 'agent/one', folder: b }])
    expect(canDrop(doc, move('agent/one', { _tag: 'Into', folder: b }))).toEqual({ refused: 'This item is no longer available.' })
  })
  it('Collapse and expand are local view intent for folders and the Unfiled group', () => {
    const doc = setup()
    const tree = sidebarTree(sidebarProjection(doc), roster, new Map([[a, true], ['wf/unfiled', false]]))
    expect(tree[0]).toMatchObject({ _tag: 'Folder', collapsed: true })
    expect(tree.at(-1)).toMatchObject({ _tag: 'Group', collapsed: false })
    expect(doc).toEqual(setup())
  })
  it('Show unfiled agents after the complete forest in stable host and seat key order', () => {
    const doc = apply(setup(), [{ op: 'subject.place', subject: 'agent/one', folder: null, key: 'z' }])
    const tree = sidebarTree(sidebarProjection(doc), roster, new Map())
    expect(tree.map((node) => node.id)).toEqual([a, b, 'wf/unfiled'])
    const group = tree[2]!
    expect(group._tag === 'Group' ? group.children.map((node) => node.id) : []).toEqual(['alpha.new', 'alpha.one', 'beta.one'])
  })
  it('New and returning agents use unfiled host order or their dormant stored slot without writes', () => {
    const doc = setup()
    const before = structuredClone(doc)
    expect(sidebarProjection(doc).unfiled).toEqual(['agent/new'])
    const returning = { ...doc, roster: [...roster, { id: 'alpha.dormant', host: 'alpha', label: 'Returning', subject: 'agent/dormant' }] }
    expect(sidebarProjection(returning).folders[0]?.members).toEqual(['agent/one', 'agent/two', 'agent/dormant'])
    expect(doc).toEqual(before)
  })
  it('Retired seats remain visible, absent archived seats keep dormant keys', () => {
    const doc = setup()
    const tree = sidebarTree(sidebarProjection(doc), roster, new Map())
    const folder = tree[0]!
    expect(folder._tag === 'Folder' ? folder.children.map((node) => node.id) : []).toEqual([child, 'beta.one', 'alpha.one', 'alpha.two'])
    expect(doc.placements['agent/dormant']?.key).toBe('a2')
  })
  it('Multi-select and batch dragging are outside the single-item contract', () => {
    const doc = setup()
    expect(canDrop(doc, move('host:alpha', { _tag: 'Into', folder: a }))).toHaveProperty('refused')
    // Runtime guard also protects callers crossing the typed kit boundary.
    const items: [string] = [a]
    items.push(b)
    expect(canDrop(doc, { items, target: { _tag: 'Root', index: 0 } })).toEqual({ refused: 'Move one item at a time.' })
  })
  it('Search prunes empty folders and keeps ancestors and full sibling order for moves', () => {
    const doc = setup()
    const tree = filterSidebarTree(sidebarTree(sidebarProjection(doc), roster, new Map([[a, true]])), 'Two')
    expect(tree).toMatchObject([{ id: a, collapsed: false, children: [{ id: 'alpha.two' }] }])
    expect(doc).toEqual(setup())
    expect(operationsForMove(doc, move('alpha.two', { _tag: 'Before', sibling: 'beta.one' }))).toHaveLength(1)
  })
  it('Mouse drop refuses cross-partition gaps, group targets and gaps between shared seats', () => {
    const doc = setup()
    for (const intent of [move(a, { _tag: 'After', sibling: 'alpha.two' }), move('alpha.two', { _tag: 'Before', sibling: child }), move(a, { _tag: 'Unfiled' }), move(a, { _tag: 'Before', sibling: 'host:alpha' }), move('alpha.two', { _tag: 'After', sibling: 'beta.one' }), move('alpha.two', { _tag: 'Before', sibling: 'alpha.one' })]) {
      expect(canDrop(doc, intent)).toHaveProperty('refused')
      expect(operationsForMove(doc, intent)).toEqual([])
    }
  })
  it('Repeated moves preserve unaffected keys and atomically include collision rekeys', () => {
    let doc = setup()
    const childKey = doc.folders[child]?.position?.key
    for (let i = 0; i < 80; i++) {
      doc = apply(doc, operationsForMove(doc, move('alpha.two', { _tag: i % 2 === 0 ? 'Before' : 'After', sibling: i % 2 === 0 ? 'beta.one' : 'alpha.one' })))
      expect(doc.folders[child]?.position?.key).toBe(childKey)
      expect(doc.placements['agent/one']?.key).toBe('a0')
    }
    const collided = setup()
    collided.placements['agent/two']!.key = 'a0'
    const operations = operationsForMove(collided, move('alpha.new', { _tag: 'Before', sibling: 'alpha.two' }))
    expect(operations.some((operation) => operation.op === 'subject.place' && operation.subject === 'agent/two')).toBe(true)
    expect(sidebarProjection(apply(collided, operations)).folders[0]?.members).toEqual(['agent/one', 'agent/new', 'agent/two'])
  })
  it('Row refusal uses fixed copy and retry only after fresh state is ready', () => {
    const targets = new Map([['alpha.one', 'agent/one'], ['beta.one', 'agent/one']])
    const refusal = { reason: { _tag: 'Known', code: 'stale-fence' }, detail: 'private server detail', targets: ['agent/one'] } as const
    const retry = () => undefined
    expect(rowStatus({ phase: 'pending', retryReady: false }, targets).get('alpha.one')).toEqual({ _tag: 'Pending' })
    expect(rowStatus({ phase: 'refused', refusal, retryReady: false, onRetry: retry }, targets).get('alpha.one')).toEqual({ _tag: 'Refused', reason: 'The folder layout changed elsewhere; refresh and try again.' })
    expect(rowStatus({ phase: 'refused', refusal, retryReady: true, onRetry: retry }, targets).get('beta.one')).toMatchObject({ onRetry: retry })
  })
  it('Generated keys are st ArrangementKey values, rekeying legacy or colliding neighbours in the same edit', () => {
    expect(nativeInsertAt([], 0).key).toBe('a0')
    const legacy = nativeInsertAt(['V', 'W'], 1)
    expect(legacy.rekeyed.map(([index]) => index)).toEqual([0, 1])
    const doc = setup()
    doc.placements['agent/one']!.key = 'V'
    doc.placements['agent/two']!.key = 'V'
    const operations = operationsForMove(doc, move('alpha.new', { _tag: 'After', sibling: 'alpha.one' }))
    expect(admissible(operations)).toBe(true)
    expect(sidebarProjection(apply(doc, operations)).folders[0]?.members).toEqual(['agent/one', 'agent/new', 'agent/two'])
    expect(admissible(operationsForCreate(setup(), { parent: null, name: 'Next' }, fresh))).toBe(true)
  })
  it('Dormant stored placements reserve their keys, so appending or rekeying never collides with them', () => {
    const doc = setup()
    const operations = operationsForMove(doc, move('alpha.new', { _tag: 'Into', folder: a }))
    const next = apply(doc, operations)
    expect(next.placements['agent/new']?.key).not.toBe(next.placements['agent/dormant']?.key)
    expect(sidebarProjection(next).folders[0]?.members).toEqual(['agent/one', 'agent/two', 'agent/new'])
    const legacy = setup()
    legacy.placements['agent/one']!.key = 'V'
    legacy.placements['agent/two']!.key = 'V'
    legacy.placements['agent/dormant']!.key = 'a0'
    const rekeyed = apply(legacy, operationsForMove(legacy, move('alpha.new', { _tag: 'After', sibling: 'alpha.one' })))
    const keys = ['agent/one', 'agent/two', 'agent/new', 'agent/dormant'].map((subject) => rekeyed.placements[subject]!.key)
    expect(new Set(keys).size).toBe(keys.length)
    expect(sidebarProjection(rekeyed).folders[0]?.members).toEqual(['agent/one', 'agent/new', 'agent/two'])
  })
  it('Restaging returns a fixed refusal for deleted or cyclic targets and Satisfied for an already applied intent', () => {
    const doc = setup()
    const into = move('alpha.two', { _tag: 'Into', folder: b })
    expect(restageMove(apply(doc, operationsForDelete(doc, { id: b })), into)).toEqual({ _tag: 'Refused', sentence: 'This folder is no longer available.' })
    const nested = apply(doc, operationsForMove(doc, move(b, { _tag: 'Into', folder: child })))
    expect(restageMove(nested, move(a, { _tag: 'Into', folder: b }))).toEqual({ _tag: 'Refused', sentence: 'A folder cannot go inside itself or its subfolders.' })
    expect(restageMove(apply(doc, operationsForMove(doc, into)), into)).toEqual({ _tag: 'Satisfied' })
    expect(restageCreate(apply(doc, operationsForCreate(doc, { parent: null, name: 'Once' }, fresh)), { parent: null, name: 'Once' }, fresh)).toEqual({ _tag: 'Satisfied' })
    expect(restageDelete(apply(doc, operationsForDelete(doc, { id: b })), { id: b })).toEqual({ _tag: 'Satisfied' })
    expect(restageRename(doc, { id: b, name: 'B' })).toEqual({ _tag: 'Satisfied' })
  })
  it('Restaging an agent move refuses once its row key is bound to another subject', () => {
    const doc = setup()
    const intent = move('alpha.two', { _tag: 'Into', folder: b })
    const bound = boundRows(doc.roster, intent)
    expect(restageBoundMove(doc, intent, bound)).toMatchObject({ _tag: 'Operations', operations: [{ subject: 'agent/two' }] })
    const rebound = { ...doc, roster: doc.roster.map((seat) => seat.id === 'alpha.two' ? { ...seat, subject: 'agent/new' } : seat) }
    expect(restageBoundMove(rebound, intent, bound)).toEqual({ _tag: 'Refused', sentence: 'This agent changed; select it again.' })
  })
})

it.live('Any sequence of valid moves keeps raw parents acyclic, partition keys unique and lands at the intended slot', () => Effect.sync(() => {
  fc.assert(fc.property(fc.array(fc.tuple(fc.integer({ min: 0, max: 5 }), fc.integer({ min: 0, max: 7 }), fc.boolean()), { maxLength: 100 }), (steps) => {
    let doc = setup()
    const ids = [a, b, child, 'beta.one', 'alpha.two', 'alpha.new']
    for (const [item, destination, after] of steps) {
      const target: SidebarMove['target'] = destination < 3 ? { _tag: 'Into', folder: ids[destination]! }
        : destination === 3 ? { _tag: 'Unfiled' }
        : destination === 4 ? { _tag: 'Root', index: after ? [...drawnParents(doc).values()].filter((parent) => parent === null).length : 0 }
        : { _tag: after ? 'After' : 'Before', sibling: ids[destination - 3]! }
      const intent = move(ids[item]!, target)
      if ('refused' in canDrop(doc, intent)) continue
      const roots = [...drawnParents(doc)].filter(([, parent]) => parent === null).map(([id]) => id)
        .sort((left, right) => {
          const x = doc.folders[left]!.position!.key
          const y = doc.folders[right]!.position!.key
          return x < y ? -1 : x > y ? 1 : left < right ? -1 : left > right ? 1 : 0
        })
      const from = roots.indexOf(ids[item]!)
      const rootIndex = target._tag === 'Root' ? target.index - (from >= 0 && from < target.index ? 1 : 0) : undefined
      const operations = operationsForMove(doc, intent)
      expect(operations.length === 0 || admissible(operations)).toBe(true)
      doc = apply(doc, operations)
      const order = (left: { id: string; key: string }, right: { id: string; key: string }) =>
        left.key < right.key ? -1 : left.key > right.key ? 1 : left.id < right.id ? -1 : left.id > right.id ? 1 : 0
      // Raw stored parents, tombstones included, never loop.
      for (const id of Object.keys(doc.folders)) {
        const seen = new Set<string>()
        for (let at: string | null | undefined = id; at != null; at = doc.folders[at]?.position?.parent) {
          expect(seen.has(at)).toBe(false)
          seen.add(at)
        }
      }
      // Keys are unique within each stored partition, dormant placements included; Unfiled is host-ordered at a0.
      const partitions = new Map<string, string[]>()
      for (const [id, parent] of drawnParents(doc)) partitions.set(`folder:${parent}`, [...partitions.get(`folder:${parent}`) ?? [], doc.folders[id]!.position!.key])
      for (const value of Object.values(doc.placements)) {
        const parent = liveAncestor(doc, value.folder)
        if (parent !== null) partitions.set(`agent:${parent}`, [...partitions.get(`agent:${parent}`) ?? [], value.key])
      }
      for (const keys of partitions.values()) expect(new Set(keys).size).toBe(keys.length)
      if (operations.length === 0 || target._tag === 'Unfiled') continue
      // The moved item lands at the planned slot among its visible siblings.
      const subjectOf = (id: string) => doc.roster.find((seat) => seat.id === id)?.subject ?? id
      const moved = subjectOf(ids[item]!)
      const isFolder = doc.folders[moved] !== undefined
      const parent = isFolder ? drawnParents(doc).get(moved) ?? null : liveAncestor(doc, doc.placements[moved]?.folder ?? null)
      const live = new Set(doc.roster.map((seat) => seat.subject))
      const visible = (isFolder
        ? [...drawnParents(doc)].filter(([, p]) => p === parent).map(([id]) => ({ id, key: doc.folders[id]!.position!.key }))
        : Object.entries(doc.placements).filter(([subject, value]) => live.has(subject) && liveAncestor(doc, value.folder) === parent).map(([id, value]) => ({ id, key: value.key }))
      ).sort(order).map((row) => row.id)
      const at = visible.indexOf(moved)
      if (target._tag === 'Into') expect(at).toBe(visible.length - 1)
      else if (target._tag === 'Root') expect(at).toBe(rootIndex)
      else if (target._tag === 'Before') expect(visible[at + 1]).toBe(subjectOf(target.sibling))
      else expect(visible[at - 1]).toBe(subjectOf(target.sibling))
    }
  }), { numRuns: 120 })
}))
