import { test } from 'node:test'
import assert from 'node:assert/strict'
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { spawn } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { storyIdsFromIndex } from './kit-play-index.mjs'
import { commitSetCases, diagonalCase, fullCases, BOTH_WIDTHS, MERGE_ONLY } from './kit-play-commit-set.mjs'

const story = { type: 'story', id: 'kit--sample', importPath: './Sample.stories.tsx' }
const docs = { type: 'docs', id: 'kit--docs', importPath: './Sample.mdx' }
test('validates all entries before selecting stories', () => {
  assert.deepEqual(storyIdsFromIndex({ entries: { story, docs } }), ['kit--sample'])
  for (const malformed of [
    null, [], {}, { ...story, type: undefined }, { ...story, type: 'unknown' },
    { ...story, id: undefined }, { ...story, id: ' ' }, { ...story, id: 7 },
    { ...story, importPath: undefined }, { ...story, importPath: '' },
    { ...docs, importPath: undefined }, { ...docs, id: '' },
  ]) {
    assert.throws(() => storyIdsFromIndex({ entries: { story, malformed } }), /Invalid Storybook index entry/)
  }
})
test('selects a tagged set from the validated index and fails on an empty one', () => {
  const critical = { ...story, id: 'kit--critical', tags: ['dev', 'critical'] }
  const other = { ...story, id: 'kit--other', tags: ['dev'] }
  const entries = { critical, other, story, docs }
  assert.deepEqual(storyIdsFromIndex({ entries }), ['kit--critical', 'kit--other', 'kit--sample'])
  assert.deepEqual(storyIdsFromIndex({ entries }, { tag: 'critical' }), ['kit--critical'])
  assert.throws(() => storyIdsFromIndex({ entries: { other, story } }, { tag: 'critical' }), /no stories tagged "critical"/)
  for (const tags of ['critical', [''], [7], null]) {
    assert.throws(() => storyIdsFromIndex({ entries: { critical, malformed: { ...other, tags } } }, { tag: 'critical' }), /tags must be an array/)
  }
})
test('rejects malformed containers, empty story sets and duplicate IDs', () => {
  for (const index of [null, [], {}, { entries: [] }, { entries: null }]) {
    assert.throws(() => storyIdsFromIndex(index), /entries object/)
  }
  for (const entries of [{}, { docs }, { first: story, second: { ...story } }]) {
    assert.throws(() => storyIdsFromIndex({ entries }), /unique, nonempty story IDs/)
  }
})
test('malformed built index fails the actual gate before browser execution', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'kit-play-index-test-'))
  try {
    const build = join(directory, 'build'), output = join(directory, 'receipt')
    await mkdir(build)
    await writeFile(join(build, 'index.json'), JSON.stringify({ entries: {
      story,
      malformed: { id: 'kit--silently-dropped-before', importPath: './Broken.stories.tsx' },
    } }))
    const child = spawn(process.execPath, [fileURLToPath(new URL('./kit-play-gate.mjs', import.meta.url)), build, output], {
      env: { ...process.env, PLAYWRIGHT_MODULE: 'must-not-import-browser-for-invalid-index' },
      stdio: 'ignore',
    })
    const code = await new Promise((resolve, reject) => {
      child.once('error', reject)
      child.once('close', resolve)
    })
    assert.equal(code, 1)
    const receipt = JSON.parse(await readFile(join(output, 'receipt.json'), 'utf8'))
    assert.equal(receipt.pass, false)
    assert.equal(receipt.cases, 0)
    assert.match(receipt.fatalError, /Invalid Storybook index entry "malformed"/)
    assert.doesNotMatch(receipt.fatalError, /must-not-import-browser/)
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})
test('the critical set fails the actual gate before browser execution when nothing is tagged', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'kit-play-index-test-'))
  try {
    const build = join(directory, 'build'), output = join(directory, 'receipt')
    await mkdir(build)
    await writeFile(join(build, 'index.json'), JSON.stringify({ entries: { story: { ...story, tags: ['dev'] } } }))
    const child = spawn(process.execPath, [fileURLToPath(new URL('./kit-play-gate.mjs', import.meta.url)), build, output], {
      env: { ...process.env, KIT_PLAY_SET: 'critical', PLAYWRIGHT_MODULE: 'must-not-import-browser-for-empty-set' },
      stdio: 'ignore',
    })
    const code = await new Promise((resolve, reject) => {
      child.once('error', reject)
      child.once('close', resolve)
    })
    assert.equal(code, 1)
    const receipt = JSON.parse(await readFile(join(output, 'receipt.json'), 'utf8'))
    assert.equal(receipt.pass, false)
    assert.equal(receipt.set, 'critical')
    assert.match(receipt.fatalError, /no stories tagged "critical"/)
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})
test('the per-commit set runs one diagonal case per story, both widths for the layout stories, and omits merge-only stories', () => {
  const ids = ['a--one', 'a--three', 'b--wide', 'c--heavy']
  const { ids: kept, cases, omittedStories, omittedCases } = commitSetCases(ids, { mergeOnlyIds: ['c--heavy'], bothWidthsIds: ['b--wide'] })
  assert.deepEqual(kept, ['a--one', 'a--three', 'b--wide'])
  assert.deepEqual(omittedStories, ['c--heavy'])
  assert.equal(cases.length, 1 + 1 + 2)
  assert.deepEqual(cases.filter(({ id }) => id === 'b--wide').map(({ scheme, width }) => `${scheme}${width}`).sort(), ['dark1440', 'dark440'])
  assert.equal(omittedCases, ids.length * 4 - cases.length)
  for (const { id, scheme, width } of cases.filter(({ id }) => id !== 'b--wide')) assert.deepEqual([scheme, width], diagonalCase(id))
  // Both diagonals are used, so both schemes and both widths stay covered across a set.
  assert.deepEqual(new Set(['a--one', 'a--three'].map(id => diagonalCase(id).join())), new Set(['dark,440', 'light,1440']))
  assert.equal(fullCases(['x']).length, 4)
})
test('the per-commit set rejects a listed story that is not critical, so a rename cannot drop coverage', () => {
  assert.throws(() => commitSetCases(['a--one'], { mergeOnlyIds: ['a--renamed'], bothWidthsIds: [] }), /a--renamed, which is not a critical story/)
  assert.throws(() => commitSetCases(['a--one'], { mergeOnlyIds: [], bothWidthsIds: ['a--gone'] }), /a--gone, which is not a critical story/)
  assert.equal(new Set([...MERGE_ONLY, ...BOTH_WIDTHS]).size, MERGE_ONLY.length + BOTH_WIDTHS.length, 'a story is merge-only or both-widths, not both')
})
