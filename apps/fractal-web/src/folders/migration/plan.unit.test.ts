import { describe, expect, it } from 'vitest'
import fc from 'fast-check'
import { compareStamp, decodeDoc, drawnParents, emptyDoc, mergeDoc } from './legacy.ts'
import type { FolderDoc } from './legacy.ts'
import { importOperations } from './plan.ts'

const id = (n: number): string => `00000000-0000-7000-8000-${n.toString(16).padStart(12, '0')}`
const doc = (name: string, ms: number, deleted = false): FolderDoc => ({ folders: { [id(1)]: {
  name: { value: name, at: [ms, 0, 'writer'] }, position: { parent: null, key: 'V', at: [ms, 0, 'writer'] },
  ...(deleted ? { deleted: [ms, 0, 'writer'] as [number, number, string] } : {}),
} }, placements: {} })
const fold = (...docs: FolderDoc[]): FolderDoc => { const result = emptyDoc(); for (const value of docs) mergeDoc(result, value); return result }

describe('legacy boundary and deterministic atomic planning', () => {
  it('joins registers commutatively, associatively and idempotently', () => {
    fc.assert(fc.property(fc.array(fc.record({ text: fc.string(), time: fc.nat({ max: 100000 }), deleted: fc.boolean() }), { minLength: 3, maxLength: 3 }), (values) => {
      const replicas = values.map((value) => doc(value.text, value.time, value.deleted))
      const [a, b, c] = replicas
      if (a === undefined || b === undefined || c === undefined) throw new RangeError('Missing generated replica')
      expect(fold(a, b)).toEqual(fold(b, a))
      expect(fold(fold(a, b), c)).toEqual(fold(a, fold(b, c)))
      expect(fold(a, a)).toEqual(fold(a))
    }), { numRuns: 100 })
  })
  it('keeps permanent remove-wins tombstones despite a later rename', () => {
    const result = fold(doc('old', 1, true), doc('later', 999))
    expect(result.folders[id(1)]?.name).toBeUndefined()
    expect(result.folders[id(1)]?.deleted).toEqual([1, 0, 'writer'])
  })
  it('orders equal clocks by Unicode scalar writer order, not UTF16 order', () => {
    expect(compareStamp([1, 1, '\u{10000}'], [1, 1, '\ue000'])).toBeGreaterThan(0)
  })
  it('does not let prototype-shaped subject IDs mutate prototypes', () => {
    const value = decodeDoc(JSON.parse('{"placements":{"__proto__":{"folder":null,"key":"V","at":[1,0,"writer"]}}}'))
    const result = fold(value)
    expect(Object.hasOwn(result.placements, '__proto__')).toBe(true)
    expect(Object.getPrototypeOf(result.placements)).toBe(Object.prototype)
  })
  it('rejects unsafe clock integers rather than changing history order', () => {
    const value = doc('Folder', 1)
    value.folders[id(1)]!.name!.at[0] = Number.MAX_SAFE_INTEGER + 1
    expect(() => decodeDoc(value)).toThrow()
  })
  it('rekeys stable IDs in legacy sibling order, not object insertion order', () => {
    fc.assert(fc.property(fc.array(fc.integer({ min: 1, max: 100 }), { minLength: 1, maxLength: 30 }), (values) => {
      const folders: FolderDoc['folders'] = {}
      values.forEach((key, index) => { folders[id(index + 1)] = { name: { value: `Folder ${index}`, at: [1, 0, 'writer'] }, position: { parent: null, key: String(key), at: [1, 0, 'writer'] } } })
      const expected = Object.keys(folders).sort((a, b) => (folders[a]!.position!.key < folders[b]!.position!.key ? -1 : folders[a]!.position!.key > folders[b]!.position!.key ? 1 : a < b ? -1 : 1))
      const operations = importOperations({ folders, placements: {} }, 'Sidebar')
      const created = operations.filter((op) => op.op === 'folder.create').sort((a, b) => a.key < b.key ? -1 : 1)
      expect(created.map((op) => op.id)).toEqual(expected)
      expect(new Set(created.map((op) => op.key)).size).toBe(values.length)
      expect(importOperations({ folders: Object.fromEntries(Object.entries(folders).reverse()), placements: {} }, 'Sidebar')).toEqual(operations)
    }), { numRuns: 100 })
  })
  it('preserves nested folders, tombstones and known raw placements in one edit', () => {
    const value = fold(doc('Root', 1))
    value.folders[id(2)] = { deleted: [2, 0, 'writer'], position: { parent: id(1), key: 'W', at: [2, 0, 'writer'] } }
    value.folders[id(3)] = { name: { value: 'Child', at: [3, 0, 'writer'] }, position: { parent: id(2), key: 'X', at: [3, 0, 'writer'] } }
    value.placements['agent/example'] = { folder: id(2), key: 'Y', at: [4, 0, 'writer'] }
    const operations = importOperations(value, 'Sidebar')
    expect(operations.find((op) => op.op === 'folder.create' && op.id === id(3))).toMatchObject({ parent: id(1) })
    expect(operations.find((op) => op.op === 'subject.place')).toMatchObject({ folder: id(2), subject: 'agent/example' })
    expect(operations.at(-1)).toEqual({ op: 'folder.delete', id: id(2) })
  })
  it('resolves concurrent legacy cycles before new cycle-refusing admission', () => {
    const value = fold(doc('A', 1))
    value.folders[id(1)]!.position!.parent = id(2)
    value.folders[id(2)] = { name: { value: 'B', at: [2, 0, 'writer'] }, position: { parent: id(1), key: 'W', at: [2, 0, 'writer'] } }
    expect(drawnParents(value).get(id(2))).toBeNull()
    const operations = importOperations(value, 'Sidebar')
    expect(operations.filter((op) => op.op === 'folder.create').map((op) => [op.id, op.parent])).toEqual([[id(2), null], [id(1), id(2)]])
  })
  it('refuses partial folders, invalid names and oversize atomic imports without dropping data', () => {
    expect(() => importOperations({ folders: { [id(1)]: {} }, placements: {} }, 'Sidebar')).toThrow('incomplete')
    expect(() => importOperations(doc('', 1), 'Sidebar')).toThrow('admitted unchanged')
    expect(() => importOperations({ folders: {}, placements: Object.fromEntries(Array.from({ length: 1024 }, (_, i) => [`agent/test-${i}`, { folder: null, key: 'V', at: [1, 0, 'writer'] }])) }, 'Sidebar')).toThrow('one atomic')
  })
})
