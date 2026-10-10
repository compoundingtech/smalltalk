import assert from 'node:assert/strict'
import { mkdtempSync, mkdirSync, rmSync, writeFileSync, readFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { resolve } from 'node:path'
import { test } from 'node:test'
import { checkIndex } from './check-storybook-index.mjs'
import { appRef, composition } from '../.storybook/composition.mjs'

const manifest = { version: 1, exceptions: [{ book: 'kit', title: 'Fractal/Explore/Workbench', classification: 'explore', owner: 'fractal-web-live', reason: 'Not the production workspace.' }] }
const index = (title, tags = []) => ({ v: 5, entries: { example: { id: 'example', type: 'story', title, name: 'AllStates', tags } } })

test('canonical kit and app roots pass', () => {
  assert.equal(checkIndex(index('Fractal/Kit/Transcript'), { version: 1, exceptions: [] }, 'kit').stories, 1)
  assert.equal(checkIndex(index('Fractal/App/Workspace'), { version: 1, exceptions: [] }, 'app').stories, 1)
})
test('planted non-conforming title fails', () => {
  assert.throws(() => checkIndex(index('Fractal UI/Transcript'), { version: 1, exceptions: [] }, 'kit'), /Non-conforming/)
  assert.throws(() => checkIndex(index('Fractal/App/Workspace'), { version: 1, exceptions: [] }, 'kit'), /Non-conforming/)
})
test('planted unmanifested exploration and scale generator fail', () => {
  assert.throws(() => checkIndex(index('Fractal/Explore/Unreviewed'), manifest, 'kit'), /missing from manifest/)
  assert.throws(() => checkIndex(index('Fractal/Perf/Turns'), { version: 1, exceptions: [] }, 'kit'), /missing from manifest/)
  assert.throws(() => checkIndex(index('Fractal/Kit/Unreviewed', ['noncanonical']), { version: 1, exceptions: [] }, 'kit'), /missing from manifest/)
  assert.throws(() => checkIndex(index('Fractal/Explore/Workbench'), { version: 1, exceptions: [] }, 'kit'), /missing from manifest/)
  assert.equal(checkIndex(index('Fractal/Explore/Workbench'), manifest, 'kit').stories, 1)
})
test('schema, ownership, duplicates, stale rows and classification fail closed', () => {
  const raw = (change) => ({ ...manifest, exceptions: [{ ...manifest.exceptions[0], ...change }] })
  assert.throws(() => checkIndex(index('Fractal/Explore/Workbench'), raw({ owner: undefined }), 'kit'))
  assert.throws(() => checkIndex(index('Fractal/Explore/Workbench'), raw({ owner: ' ' }), 'kit'), /Empty/)
  assert.throws(() => checkIndex(index('Fractal/Explore/Workbench'), raw({ reason: '' }), 'kit'), /Empty/)
  assert.throws(() => checkIndex(index('Fractal/Explore/Workbench'), raw({ extra: true }), 'kit'))
  assert.throws(() => checkIndex(index('Fractal/Explore/Workbench'), { ...manifest, exceptions: [...manifest.exceptions, ...manifest.exceptions] }, 'kit'), /Duplicate/)
  assert.throws(() => checkIndex(index('Fractal/Kit/Transcript'), manifest, 'kit'), /Stale/)
  assert.throws(() => checkIndex(index('Fractal/Explore/Workbench'), raw({ classification: 'perf' }), 'kit'), /classification/)
  assert.throws(() => checkIndex({ v: 5, entries: {} }, { version: 1, exceptions: [] }, 'kit'), /Empty/)
})
test('local missing ref is lazy; static build requires output once app book exists', () => {
  const repoRoot = mkdtempSync(resolve(tmpdir(), 'fractal-book-contract-'))
  try {
    const options = { repoRoot, build: false, ci: false, appUrl: 'https://app.example.test' }
    assert.equal(composition(options).refs[appRef.id].url, options.appUrl)
    assert.equal(composition(options).refs[appRef.id].type, 'server-lazy')
    assert.equal(composition({ ...options, build: true }).refs[appRef.id].url, appRef.staticPath)
    assert.equal(composition({ ...options, ci: true }).refs[appRef.id].url, appRef.staticPath)
    assert.deepEqual(composition({ ...options, build: true }).staticDirs, [])
    mkdirSync(resolve(repoRoot, 'apps/fractal-web/.storybook'), { recursive: true })
    assert.doesNotThrow(() => composition(options))
    assert.throws(() => composition({ ...options, build: true, ci: true }), /static output is missing/)
    const output = resolve(repoRoot, 'apps/fractal-web/storybook-static')
    mkdirSync(output)
    writeFileSync(resolve(output, 'index.json'), '{}')
    assert.throws(() => composition({ ...options, build: true }), /static output is missing/)
    writeFileSync(resolve(output, 'index.html'), '<html></html>')
    assert.deepEqual(composition({ ...options, build: true }).staticDirs, [{ from: output, to: '/apps/fractal-web/storybook-static' }])
  } finally {
    rmSync(repoRoot, { recursive: true, force: true })
  }
})
test('CI owns one serial build, app before landing, with index controls wired', () => {
  const script = readFileSync(new URL('../../../scripts/ci-fractal-web-storybooks', import.meta.url), 'utf8')
  assert.ok(script.indexOf('pnpm --filter fractal-web build-storybook') < script.indexOf('pnpm --filter @smalltalk/fractal-ui build-storybook'))
  assert.match(script, /node --test packages\/fractal-ui\/scripts\/check-storybook-index.test.mjs/)
  assert.match(script, /check-storybook-index.mjs app/)
  assert.match(script, /check-storybook-index.mjs kit/)
  const lanes = readFileSync(new URL('../../../scripts/ci-fractal-web', import.meta.url), 'utf8')
  assert.match(lanes, /lane "Composed Fractal Storybooks" bash scripts\/ci-fractal-web-storybooks/)
})
