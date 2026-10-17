/**
 * The only writer of `fixtures/scenarios`. `--check` generates into a temporary directory and
 * fails on any byte difference, naming each stale, missing or extra file.
 */
import { existsSync, mkdtempSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, relative } from 'node:path'
import { fileURLToPath } from 'node:url'

import { ANCHOR, ANCHOR_MS, catalog, loadWorld, SLICE_KINDS, wireValues, type AnySlice, type TimePointer, type World } from '../src/index.ts'
import { loadDefinitions, makeInstantFinder } from './times.ts'
import { rebaseVectors } from './vectors.ts'
import { decodeRecording, decodeScript, normalizeRecording } from './recordings.ts'
import { NARRATIVE_RECORDINGS, narrativeScript } from './narrative-scripts.ts'

export const FIXTURES_DIR = fileURLToPath(new URL('../../../../fixtures/scenarios/', import.meta.url))
export const SLICE_FORMAT = 'st3.scenario.slice.v1'
export const RECORDINGS_DIR = fileURLToPath(new URL('../recordings/', import.meta.url))

/** Raw recordings and the version-1 proof script are inputs; narrative scripts are generated. */
export const generateRecordings = (root: string = RECORDINGS_DIR): Map<string, string> => {
  const files = new Map<string, string>()
  const generated = new Set<string>()
  for (const id of NARRATIVE_RECORDINGS) {
    const name = `${id}.script.json`
    generated.add(name)
    const raw = join(root, `${id}.recording.jsonl`)
    const rows = existsSync(raw) ? decodeRecording(readFileSync(raw, 'utf8')) : []
    const script = narrativeScript(id, rows)
    files.set(name, canonicalJson(script))
    if (rows.length) files.set(`${id}.normalized.json`, canonicalJson(normalizeRecording(script, rows, ANCHOR_MS)))
  }
  for (const name of readdirSync(root).filter((name) => name.endsWith('.script.json') && !generated.has(name)).sort()) {
    const stem = name.slice(0, -'.script.json'.length)
    const script = decodeScript(JSON.parse(readFileSync(join(root, name), 'utf8')))
    const rows = decodeRecording(readFileSync(join(root, `${stem}.recording.jsonl`), 'utf8'))
    files.set(`${stem}.normalized.json`, canonicalJson(normalizeRecording(script, rows, ANCHOR_MS)))
  }
  return files
}

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
  readonly unknown?: AnySlice['unknown']
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
  ...(slice.unknown === undefined ? {} : { unknown: slice.unknown }),
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

/** Generated narrative scripts and normalized outputs both participate in freshness. */
export const checkRecordings = (root: string = RECORDINGS_DIR): FreshnessProblem[] => {
  const files = generateRecordings(root)
  const actual = new Set(readdirSync(root).filter((name) => name.endsWith('.normalized.json') || NARRATIVE_RECORDINGS.some((id) => name === `${id}.script.json`)))
  const problems: FreshnessProblem[] = []
  for (const [path, bytes] of files) {
    if (!actual.has(path)) problems.push({ path, problem: 'missing' })
    else if (readFileSync(join(root, path), 'utf8') !== bytes) problems.push({ path, problem: 'stale' })
    actual.delete(path)
  }
  for (const path of actual) problems.push({ path, problem: 'extra' })
  return problems
}

if (import.meta.main) {
  if (process.argv.includes('--check')) {
    const problems = checkTree(FIXTURES_DIR)
    const recordings = checkRecordings()
    for (const { path, problem } of problems) console.error(`${problem}: fixtures/scenarios/${path}`)
    for (const { path, problem } of recordings) console.error(`${problem}: recordings/${path}`)
    if (problems.length > 0 || recordings.length > 0) {
      console.error('fixtures/scenarios is out of date; run: pnpm --filter @smalltalk/st3-scenarios emit')
      process.exitCode = 1
    } else console.log('emit --check: fixtures/scenarios is up to date')
  } else {
    if (!process.argv.includes('--recordings-only')) rmSync(FIXTURES_DIR, { recursive: true, force: true })
    if (!process.argv.includes('--recordings-only')) {
      const files = generate()
      writeTree(FIXTURES_DIR, files)
      console.log(`emit: wrote ${files.size} files to fixtures/scenarios`)
    }
    const recordings = generateRecordings()
    for (const name of readdirSync(RECORDINGS_DIR).filter((name) => name.endsWith('.normalized.json'))) {
      if (!recordings.has(name)) rmSync(join(RECORDINGS_DIR, name))
    }
    writeTree(RECORDINGS_DIR, recordings)
    console.log(`emit: wrote ${recordings.size} generated recording files`)
  }
}
