import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { test } from 'node:test'
import {
  FolderValidationError, Hlc, applyOp, claimFields, claimKind, compareStamp,
  createFolder, decodeClaim, decodeDoc, decodeOps, deleteFolder, docKey,
  drawnParents, emptyDoc, folderId, insertAt, isLive, keyBetween, liveAncestor,
  maxStamp, mergeDoc, moveFolder, opsKey, place, project, renameFolder,
  serializeFields, serializeOps, wouldCycle,
} from './core.mts'

const at = (ms, writer = 'a', counter = 0) => [ms, counter, writer]
const docOf = (ops) => {
  const doc = emptyDoc()
  for (const op of ops) applyOp(doc, op)
  return doc
}
const merged = (a, b) => { const doc = decodeDoc(a); mergeDoc(doc, b); return doc }
const paths = (doc) => {
  const result = []
  const walk = (folders, prefix) => {
    for (const folder of folders) {
      const path = prefix ? `${prefix}/${folder.name}` : folder.name
      result.push(path)
      walk(folder.folders, path)
    }
  }
  walk(project(doc, []).folders, '')
  return result
}

test('HLC survives clock rollback, restart, remote observation, and counter overflow', () => {
  assert.ok(compareStamp(at(1, 'z', 9), at(2)) < 0)
  assert.ok(compareStamp(at(2, 'z'), at(2, 'a', 1)) < 0)
  assert.ok(compareStamp(at(2, 'a', 1), at(2, 'b', 1)) < 0)
  const clock = new Hlc('local')
  assert.deepEqual(clock.stamp(1000), at(1000, 'local'))
  assert.deepEqual(clock.stamp(1000), at(1000, 'local', 1))
  assert.deepEqual(clock.stamp(900), at(1000, 'local', 2))
  clock.observe(at(2000, 'remote', 12))
  clock.observe(at(1999, 'older', 99))
  assert.deepEqual(clock.stamp(900), at(2000, 'local', 13))
  const doc = docOf([createFolder('f', 'f', null, 'V', at(5000, 'remote', 8)), place('agent/x/a', null, 'V', at(6000)), deleteFolder('gone', at(7000))])
  clock.observe(maxStamp(doc))
  assert.ok(compareStamp(clock.stamp(1000), maxStamp(doc)) > 0)
  clock.observe(at(8000, 'remote', 0xffffffff))
  assert.deepEqual(clock.stamp(1000), at(8001, 'local'))
})

test('UUIDv7 has exactly the Rust timestamp and random-bit encoding', () => {
  const random = Uint8Array.from([0x0c, 0x3e, 0x1f, 0x00, 0x5b, 0x1d, 0x2e, 0x3f, 0x4a, 0x5b])
  assert.equal(folderId(0x0192f0c47a1e, random), '0192f0c4-7a1e-7c3e-9f00-5b1d2e3f4a5b')
  assert.ok(folderId(1000, random) < folderId(1001, new Uint8Array(10)))
})

test('early rename/move and concurrent independent registers survive an older create', () => {
  const ops = [
    createFolder('g', 'g', null, 'V', at(1)),
    renameFolder('f', 'renamed', at(20, 'b')),
    moveFolder('f', 'g', 'W', at(21, 'b')),
    createFolder('f', 'original', null, 'V', at(10)),
  ]
  const left = docOf(ops)
  const right = docOf([...ops].reverse())
  assert.deepEqual(left, right)
  assert.deepEqual(paths(left), ['g', 'g/renamed'])
  assert.equal(applyOp(left, ops[2]), false)
})

test('deletion is permanent irrespective of later names and positions, retaining ancestry', () => {
  const doc = docOf([
    createFolder('top', 'top', null, 'V', at(1)),
    createFolder('middle', 'middle', 'top', 'V', at(2)),
    createFolder('inner', 'inner', 'middle', 'V', at(3)),
    createFolder('nested', 'nested', 'inner', 'V', at(4)),
    place('agent/x/a', 'middle', 'V', at(5)),
    place('agent/x/b', 'inner', 'W', at(6)),
    renameFolder('middle', 'later', at(100)),
    deleteFolder('middle', at(7)), deleteFolder('inner', at(8)),
  ])
  assert.equal(doc.folders.middle.name, undefined)
  assert.equal(applyOp(doc, renameFolder('middle', 'revive', at(200))), false)
  assert.equal(isLive(doc, 'middle'), false)
  assert.equal(liveAncestor(doc, 'inner'), 'top')
  assert.deepEqual(paths(doc), ['top', 'top/nested'])
  assert.deepEqual(project(doc, ['agent/x/b', 'agent/x/a']).folders[0].members, ['agent/x/a', 'agent/x/b'])
  applyOp(doc, deleteFolder('top', at(9)))
  assert.deepEqual(project(doc, ['agent/x/b', 'agent/x/a']).unfiled, ['agent/x/b', 'agent/x/a'])
  assert.deepEqual(paths(doc), ['nested'])
  assert.equal(doc.placements['agent/x/b'].folder, 'inner')
  applyOp(doc, createFolder('middle', 'still gone', null, 'X', at(300)))
  assert.equal(isLive(doc, 'middle'), false)
})

test('projection preserves dormant placements, resolves unknowns, and orders ties and unfiled', () => {
  const doc = docOf([
    createFolder('b', 'b', null, 'V', at(1)), createFolder('a', 'a', null, 'V', at(1)),
    place('agent/x/m', 'a', 'W', at(2)), place('agent/x/b', 'a', 'V', at(2)),
    place('agent/x/a', 'a', 'V', at(2)), place('agent/x/dormant', 'a', 'X', at(2)),
    place('agent/x/unknown', 'missing', 'V', at(2)),
  ])
  const live = ['agent/x/z', 'agent/x/m', 'agent/x/b', 'agent/x/y', 'agent/x/a', 'agent/x/unknown']
  const view = project(doc, live)
  assert.deepEqual(view.folders.map((folder) => [folder.id, folder.members]), [['a', ['agent/x/a', 'agent/x/b', 'agent/x/m']], ['b', []]])
  assert.deepEqual(view.unfiled, ['agent/x/z', 'agent/x/y', 'agent/x/unknown'])
  assert.deepEqual(project(doc, ['agent/x/dormant']).folders[0].members, ['agent/x/dormant'])
  applyOp(doc, createFolder('missing', 'arrived', null, 'W', at(1)))
  assert.deepEqual(project(doc, live).folders[2].members, ['agent/x/unknown'])
  applyOp(doc, place('agent/x/a', 'b', 'V', at(3)))
  assert.equal(applyOp(doc, place('agent/x/a', 'a', 'V', at(2, 'z', 9))), false)
  assert.deepEqual(project(doc, live).folders[1].members, ['agent/x/a'])
  applyOp(doc, place('agent/x/a', null, 'V', at(4)))
  assert.deepEqual(project(doc, ['agent/x/a']).unfiled, ['agent/x/a'])
})

test('concurrent cycles break at greatest position stamp, then ID, independent of delivery', () => {
  const base = docOf(['a', 'b', 'c'].map((id) => createFolder(id, id, null, 'V', at(1))))
  const moves = [moveFolder('a', 'b', 'V', at(10, 'site-a')), moveFolder('b', 'c', 'V', at(12, 'site-b')), moveFolder('c', 'a', 'V', at(11, 'site-c'))]
  for (const order of [[0, 1, 2], [2, 1, 0], [1, 0, 2], [2, 0, 1]]) {
    const doc = decodeDoc(base)
    for (const index of order) applyOp(doc, moves[index])
    assert.deepEqual(paths(doc), ['b', 'b/a', 'b/a/c'])
    assert.equal(wouldCycle(doc, 'b', 'c'), true)
    assert.equal(wouldCycle(doc, 'c', 'b'), false)
    assert.equal(wouldCycle(doc, 'a', null), false)
  }
  const tied = docOf([createFolder('a', 'a', 'b', 'V', at(1)), createFolder('b', 'b', 'a', 'V', at(1))])
  assert.equal(drawnParents(tied).get('b'), null)
  assert.deepEqual(paths(tied), ['b', 'b/a'])
  const deletedLoop = docOf([createFolder('x', 'x', 'y', 'V', at(1)), createFolder('y', 'y', 'x', 'V', at(1)), deleteFolder('x', at(2)), deleteFolder('y', at(2)), place('agent/x/a', 'x', 'V', at(3))])
  assert.deepEqual(project(deletedLoop, ['agent/x/a']), { folders: [], unfiled: ['agent/x/a'] })
})

test('equal-stamp content tie-breaks use Rust Option and Unicode scalar ordering', () => {
  const ops = [createFolder('f', '\u{10000}', null, 'V', at(1)), createFolder('f', '\ue000', 'g', 'A', at(1)), place('agent/x/a', null, 'z', at(1)), place('agent/x/a', 'f', 'A', at(1))]
  const doc = docOf(ops)
  assert.deepEqual(doc, docOf([...ops].reverse()))
  assert.equal(doc.folders.f.name.value, '\u{10000}')
  assert.equal(doc.folders.f.position.parent, 'g')
  assert.equal(doc.placements['agent/x/a'].folder, 'f')
  assert.ok(compareStamp(at(1, '\u{10000}'), at(1, '\ue000')) > 0)
  const special = docOf([createFolder('__proto__', 'safe', null, 'V', at(1)), place('constructor', '__proto__', 'V', at(1))])
  assert.deepEqual(project(special, ['constructor']).folders[0].members, ['constructor'])
})

test('wire decoding preserves serde defaults and rejects corrupt registers at the boundary', () => {
  assert.deepEqual(decodeDoc({ v: 1 }), emptyDoc())
  const decoded = decodeDoc({ folders: { f: { name: null, deleted: null, position: { key: 'V', at: at(1), extra: true } } }, placements: { 'agent/x/a': { key: 'W', at: at(2) } } })
  assert.deepEqual(decoded.folders.f, { position: { parent: null, key: 'V', at: at(1) } })
  assert.equal(decoded.placements['agent/x/a'].folder, null)
  assert.deepEqual(decodeOps([{ op: 'create_folder', id: 'f', name: 'f', key: 'V', at: at(1), ignored: 'extra' }]), [createFolder('f', 'f', null, 'V', at(1))])
  for (const value of [null, [], { folders: null }, { folders: { f: { position: { key: 'V', at: [1, -1, 'w'] } } } }, { placements: { a: { key: 'V', at: [Number.MAX_SAFE_INTEGER + 1, 0, 'w'] } } }]) assert.throws(() => decodeDoc(value), FolderValidationError)
  for (const value of [{ ops: [] }, [place('a', null, 'V', [1, 0x100000000, 'w'])], [{ op: 'future', id: 'f', at: at(1) }], [renameFolder('f', '\ud800', at(1))], [moveFolder('f', 42, 'V', at(1))], [deleteFolder('f', [1, 0, 'w', 'extra'])]]) assert.throws(() => decodeOps(value), FolderValidationError)
})

test('claim decoding admits person/fleet actors only and skips unrelated or invalid claims', () => {
  const doc = docOf([createFolder('f', 'f', null, 'V', at(1))])
  const claim = { store_index: 1, kind: claimKind, actor: 'person/ada', body: { fields: claimFields(doc) } }
  assert.deepEqual(decodeClaim(claim), doc)
  assert.deepEqual(decodeClaim({ ...claim, actor: 'agent/setup/fractal' }), doc)
  for (const actor of [undefined, null, 'daemon/site-a', 'system', 'agent/', 'person/', 'person']) assert.equal(decodeClaim({ ...claim, actor }), undefined)
  for (const value of [null, {}, { ...claim, kind: 'custom.other' }, { ...claim, body: {} }, { ...claim, body: { fields: { v: 2 } } }, { ...claim, body: { fields: { v: 1, folders: null } } }]) assert.equal(decodeClaim(value), undefined)
})

test('ops and republish keys hash exact Rust serialization, not request insertion order', async () => {
  const ops = [createFolder('f', 'line\n"é', null, 'V', at(1)), renameFolder('f', 'new', at(2)), moveFolder('f', 'g', 'W', at(3)), deleteFolder('g', at(4)), place('agent/x/a', null, 'V', at(5))]
  const expected = '[{"op":"create_folder","id":"f","name":"line\\n\\"é","parent":null,"key":"V","at":[1,0,"a"]},{"op":"rename_folder","id":"f","name":"new","at":[2,0,"a"]},{"op":"move_folder","id":"f","parent":"g","key":"W","at":[3,0,"a"]},{"op":"delete_folder","id":"g","at":[4,0,"a"]},{"op":"place","agent":"agent/x/a","folder":null,"key":"V","at":[5,0,"a"]}]'
  assert.equal(serializeOps(ops), expected)
  const subject = 'custom/fractal/sidebar-operator'
  const expectedKey = `fractal.sidebar:${subject}:ops:${createHash('sha256').update(expected).digest('hex').slice(0, 32)}`
  assert.equal(await opsKey(subject, ops), expectedKey)
  const reordered = ops.map((op) => Object.fromEntries(Object.entries(op).reverse()))
  assert.equal(await opsKey(subject, reordered), expectedKey)
  const doc = docOf([createFolder('2', 'two', null, 'V', at(1)), createFolder('10', 'ten', null, 'W', at(2))])
  const fields = '{"folders":{"10":{"name":{"at":[2,0,"a"],"value":"ten"},"position":{"at":[2,0,"a"],"key":"W","parent":null}},"2":{"name":{"at":[1,0,"a"],"value":"two"},"position":{"at":[1,0,"a"],"key":"V","parent":null}}},"placements":{},"v":1}'
  assert.equal(serializeFields(doc), fields)
  assert.equal(await docKey(subject, doc), `fractal.sidebar:${subject}:doc:${createHash('sha256').update(fields).digest('hex').slice(0, 32)}`)
})

test('fractional keys fit tight gaps and keep repeated insertion growth bounded', () => {
  assert.equal(keyBetween(), 'V')
  for (const [lower, upper] of [[null, '1'], [null, '01'], ['z', null], ['zz', null], ['1', '2'], ['1', '1V'], ['1z', '2'], ['V', 'W1'], ['0V', '1'], ['A', 'y'], [null, '10'], ['10', '2']]) {
    const key = keyBetween(lower, upper)
    assert.match(key, /^[0-9A-Za-z]*[1-9A-Za-z]$/)
    assert.ok(lower === null || lower < key)
    assert.ok(upper === null || key < upper)
  }
  for (const [lower, upper] of [['V', 'V'], ['W', 'V'], ['1', '10'], ['V', '000']]) assert.ok(lower < keyBetween(lower, upper))
  assert.throws(() => keyBetween(null, '~'), RangeError)
  for (const next of [(key) => keyBetween(null, key), (key) => keyBetween(key, null), (key) => keyBetween('V', key === 'V' ? null : key), (key) => keyBetween(key, 'W')]) {
    let key = 'V'
    for (let i = 0; i < 1000; i += 1) { key = next(key); assert.ok(key.length <= 40) }
  }
})

test('insertion repairs collisions with minimal adjacent rekeys and preserves sibling order', () => {
  for (const [siblings, index, expectedRekeys] of [
    [['V', 'W'], 1, 0],
    [['V', 'V'], 1, 1],
    [['U', 'V', 'V', 'V', 'W'], 1, 0],
    [['U', 'V', 'V', 'V', 'W'], 2, 1],
    [['U', 'V', 'V', 'V', 'W'], 3, 1],
    [['V', 'V', 'V', 'V'], 1, 1],
    [['V', 'V', 'V', 'V'], 3, 1],
    [['1', '10'], 1, 1],
    [['0'], 0, 1],
    [[''], 0, 1],
    [['~', '~'], 1, 1],
  ]) {
    const plan = insertAt(siblings, index)
    assert.equal(plan.rekeyed.length, expectedRekeys)
    const result = [...siblings]
    for (const [at, key] of plan.rekeyed) result[at] = key
    result.splice(index, 0, plan.key)
    assert.ok(index === 0 || result[index - 1] < result[index])
    assert.ok(index === result.length - 1 || result[index] < result[index + 1])
  }
  // A tie in rekey count chooses the following sibling, as Rust does.
  assert.equal(insertAt(['V', 'V'], 1).rekeyed[0][0], 1)
})

test('deterministic register collisions converge under reorder, replay, and replica joins', () => {
  let seed = 42
  const next = (limit) => { seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0; return seed % limit }
  const live = ['agent/x/a', 'agent/x/b', 'agent/x/c']
  const makeOps = () => Array.from({ length: 24 }, () => {
    const id = `f${next(4)}`
    const stamp = at(next(3), ['a', 'b'][next(2)], next(2))
    const parent = next(3) === 0 ? null : `f${next(4)}`
    const key = ['V', 'W'][next(2)]
    switch (next(5)) {
      case 0: return createFolder(id, ['p', 'q'][next(2)], parent, key, stamp)
      case 1: return renameFolder(id, ['r', 's'][next(2)], stamp)
      case 2: return moveFolder(id, parent, key, stamp)
      case 3: return deleteFolder(id, stamp)
      default: return place(live[next(3)], parent, key, stamp)
    }
  })
  for (let i = 0; i < 200; i += 1) {
    const ops = makeOps()
    const expected = docOf(ops)
    const shuffled = [...ops]
    for (let j = shuffled.length - 1; j > 0; j -= 1) { const k = next(j + 1); [shuffled[j], shuffled[k]] = [shuffled[k], shuffled[j]] }
    const replayed = docOf([...shuffled, ...ops])
    assert.deepEqual(replayed, expected)
    const a = docOf(ops.slice(0, 8)), b = docOf(ops.slice(8, 16)), c = docOf(ops.slice(16))
    assert.deepEqual(merged(a, b), merged(b, a))
    assert.deepEqual(merged(merged(a, b), c), merged(a, merged(b, c)))
    assert.deepEqual(merged(a, a), a)
    assert.deepEqual(merged(merged(a, b), c), expected)
    assert.deepEqual(project(replayed, live), project(expected, live))
    assert.equal(mergeDoc(expected, replayed), false)
  }
})
