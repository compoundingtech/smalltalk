/**
 * The only writer of `fixtures/scenarios`. `--check` generates into a temporary directory and
 * fails on any byte difference, naming each stale, missing or extra file.
 */
import { mkdtempSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, relative } from 'node:path'
import { fileURLToPath } from 'node:url'

import { ANCHOR, ANCHOR_MS, catalog, loadWorld, SLICE_KINDS, wireValues, type AnySlice, type TimePointer, type World } from '../src/index.ts'
import { loadDefinitions, makeInstantFinder } from './times.ts'
import { rebaseVectors } from './vectors.ts'

export const FIXTURES_DIR = fileURLToPath(new URL('../../../../fixtures/scenarios/', import.meta.url))
export const SLICE_FORMAT = 'st3.scenario.slice.v1'

/** Sorted keys, two-space indentation, trailing newline. */
export const canonicalJson = (value: unknown): string => {
  const sort = (input: unknown): unknown => {
    if (Array.isArray(input)) return input.map(sort)
    if (input === null || typeof input !== 'object') return input
    return Object.fromEntries(
      Object.keys(input)
        .sort()
        .filter((key) => (input as Record<string, unknown>)[key] !== undefined)
        .map((key) => [key, sort((input as Record<string, unknown>)[key])]),
    )
  }
  return JSON.stringify(sort(value), null, 2) + '\n'
}

const findInstants = makeInstantFinder(loadDefinitions())

/** JSON pointers of every instant in the slice's wire values, in document order. */
export const sliceTimes = (slice: AnySlice): TimePointer[] =>
  wireValues(slice).flatMap(({ definition, value, pointer }) => findInstants(definition, value, pointer))

export interface SliceFile {
  readonly format: typeof SLICE_FORMAT
  readonly world: string
  readonly slice: string
  readonly variant: string
  readonly anchor: string
  readonly source: AnySlice['source']
  readonly decode: AnySlice['decode']
  readonly loading: boolean
  readonly times: TimePointer[]
  readonly state: unknown
  readonly timeline: unknown
}

export const sliceFile = (world: World, slice: AnySlice): SliceFile => ({
  format: SLICE_FORMAT,
  world: world.id,
  slice: slice.kind,
  variant: slice.variant,
  anchor: ANCHOR,
  source: slice.source,
  decode: slice.decode,
  loading: slice.loading,
  times: sliceTimes(slice),
  state: slice.state,
  timeline: slice.timeline,
})

/** Every committed file: relative path → bytes. */
export const generate = (): Map<string, string> => {
  const files = new Map<string, string>()
  const index = []
  for (const entry of catalog) {
    const world = loadWorld(entry.id, { now: ANCHOR_MS })
    for (const kind of SLICE_KINDS) files.set(`${world.id}/${kind}.json`, canonicalJson(sliceFile(world, world.slices[kind] as AnySlice)))
    index.push({
      id: world.id,
      title: world.title,
      narrative: world.narrative,
      seed: world.seed,
      slices: Object.fromEntries(SLICE_KINDS.map((kind) => [kind, `${world.id}/${kind}.json`])),
      cast: {
        agents: world.cast.agents.map(({ id, name }) => ({ id, name })),
        people: world.cast.people,
        hosts: world.cast.hosts.map(({ id, name }) => ({ id, name })),
      },
    })
  }
  files.set('index.json', canonicalJson({ format: 'st3.scenario.index.v1', anchor: ANCHOR, worlds: index }))
  files.set('_vectors/rebase.json', canonicalJson({ format: 'st3.scenario.rebase-vectors.v1', vectors: rebaseVectors }))
  return files
}

const listFiles = (root: string): string[] => {
  const out: string[] = []
  const visit = (dir: string) => {
    for (const name of readdirSync(dir)) {
      const path = join(dir, name)
      if (statSync(path).isDirectory()) visit(path)
      else out.push(relative(root, path))
    }
  }
  try {
    visit(root)
  } catch {
    return []
  }
  return out.sort()
}

export const writeTree = (root: string, files: Map<string, string>): void => {
  for (const [path, bytes] of files) {
    mkdirSync(dirname(join(root, path)), { recursive: true })
    writeFileSync(join(root, path), bytes)
  }
}

export interface FreshnessProblem {
  readonly path: string
  readonly problem: 'stale' | 'missing' | 'extra'
}

/** Byte comparison of a freshly generated tree with `root`. */
export const checkTree = (root: string, files: Map<string, string> = generate()): FreshnessProblem[] => {
  const scratch = mkdtempSync(join(tmpdir(), 'st3-scenarios-'))
  try {
    writeTree(scratch, files)
    const expected = listFiles(scratch)
    const actual = new Set(listFiles(root))
    const problems: FreshnessProblem[] = []
    for (const path of expected) {
      if (!actual.has(path)) problems.push({ path, problem: 'missing' })
      else if (!readFileSync(join(root, path)).equals(readFileSync(join(scratch, path)))) problems.push({ path, problem: 'stale' })
      actual.delete(path)
    }
    for (const path of actual) problems.push({ path, problem: 'extra' })
    return problems
  } finally {
    rmSync(scratch, { recursive: true, force: true })
  }
}

if (import.meta.main) {
  if (process.argv.includes('--check')) {
    const problems = checkTree(FIXTURES_DIR)
    for (const { path, problem } of problems) console.error(`${problem}: fixtures/scenarios/${path}`)
    if (problems.length > 0) {
      console.error('fixtures/scenarios is out of date; run: pnpm --filter @smalltalk/st3-scenarios emit')
      process.exitCode = 1
    } else console.log('emit --check: fixtures/scenarios is up to date')
  } else {
    rmSync(FIXTURES_DIR, { recursive: true, force: true })
    const files = generate()
    writeTree(FIXTURES_DIR, files)
    console.log(`emit: wrote ${files.size} files to fixtures/scenarios`)
  }
}
