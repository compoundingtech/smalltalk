/**
 * Decode gate (spec "Gates" 3): every wire value of every world and variant decodes with the real
 * client-v0 codecs in strict mode, at the anchor and rebased to another `now` (`huge`: every variant
 * at the anchor, default slices at the other `now`), and so does every committed slice file that
 * non-TypeScript readers load.
 */
import { readdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'

import * as Schema from '@smalltalk/st3-client/schema'

import { ANCHOR_MS, catalog, loadWorld, SLICE_KINDS, wireValues, type AnySlice, type SliceKind, type World } from '../src/index.ts'
import { FIXTURES_DIR, SLICE_FORMAT, type SliceFile } from './emit.ts'

export interface DecodeFailure {
  readonly world: string
  readonly slice: string
  readonly variant: string
  readonly pointer: string
  readonly message: string
}

/**
 * Validate the tolerant value, a strict-clean witness, then each contamination in isolation.
 * Strict open-enum failures are root filters, so diagnostic strings cannot locate them.
 */
export const decodeSlice = (world: string, slice: AnySlice): DecodeFailure[] => {
  const failures: DecodeFailure[] = []
  const declarations = slice.unknown ?? []
  const covered = new Set<string>()
  const failure = (pointer: string, message: string) => failures.push({ world, slice: slice.kind, variant: slice.variant, pointer, message })
  for (const { pointer, definition, value } of wireValues(slice)) {
    const paths = declarations.filter((path) => path.pointer === pointer || path.pointer.startsWith(`${pointer}/`))
    for (const path of paths) covered.add(path.pointer)
    const decode = (input: unknown, mode: 'strict' | 'tolerant'): string | undefined => {
      try { Schema.decodeUnknownSync(Schema[definition] as never, mode)(input); return undefined }
      catch (error) { return error instanceof Error ? error.message : String(error) }
    }
    if (paths.length === 0) {
      const error = decode(value, 'strict')
      if (error !== undefined) failure(pointer, error)
      continue
    }
    if (slice.decode !== 'tolerant') failure(pointer, 'declared unknown paths require tolerant decode')
    const tolerantError = decode(value, 'tolerant')
    if (tolerantError !== undefined) failure(pointer, tolerantError)
    try {
      const decoded: unknown = Schema.decodeUnknownSync(Schema[definition] as never, 'tolerant')(value)
      // Replacing every declaration must restore a strict-valid value. Any independent,
      // undeclared contamination still fails this check.
      const patch = (input: unknown, path: string, replacement: unknown): unknown => {
        if (path === pointer) return structuredClone(replacement)
        const tokens = path.slice(pointer.length + 1).split('/').map((token) => {
          if (/~(?![01])/u.test(token)) throw new Error('invalid JSON pointer escape')
          return token.replace(/~1/gu, '/').replace(/~0/gu, '~')
        })
        const output: unknown = structuredClone(input)
        let parent: unknown = output
        for (const token of tokens.slice(0, -1)) {
          if (parent === null || typeof parent !== 'object' || !Object.hasOwn(parent, token)) throw new Error('unknown pointer does not exist')
          parent = (parent as Record<string, unknown>)[token]
        }
        const key = tokens.at(-1)
        if (key === undefined || parent === null || typeof parent !== 'object' || !Object.hasOwn(parent, key)) throw new Error('unknown pointer does not exist')
        if (replacement === undefined) {
          if (Array.isArray(parent)) throw new Error('array contamination needs a known_value')
          delete (parent as Record<string, unknown>)[key]
        } else (parent as Record<string, unknown>)[key] = structuredClone(replacement)
        return output
      }
      for (let index = 0; index < paths.length; index += 1) {
        const path = paths[index]!
        const tokens = path.pointer === pointer ? [] : path.pointer.slice(pointer.length + 1).split('/')
          .map((token) => token.replace(/~1/gu, '/').replace(/~0/gu, '~'))
        let rawLeaf: unknown = value
        let decodedLeaf: unknown = decoded
        let decodedParent: unknown
        for (const token of tokens) {
          decodedParent = decodedLeaf
          rawLeaf = rawLeaf !== null && typeof rawLeaf === 'object' ? (rawLeaf as Record<string, unknown>)[token] : undefined
          decodedLeaf = decodedLeaf !== null && typeof decodedLeaf === 'object' ? (decodedLeaf as Record<string, unknown>)[token] : undefined
        }
        // Scalar declarations are checked by the isolated strict probe below; a known scalar
        // therefore remains a stale declaration. Composite ancestors cannot hide leaf errors.
        const scalarLeaf = rawLeaf === null || typeof rawLeaf !== 'object'
        const key = tokens.at(-1)
        const excessKey = key !== undefined && decodedParent !== null && typeof decodedParent === 'object' && !Object.hasOwn(decodedParent, key)
        const discriminator = definition === 'Resource' ? 'kind' : definition === 'TimelineEntry' ? 'type' : undefined
        const unknownMember = tokens.length === 0 && discriminator !== undefined && decodedLeaf !== null && typeof decodedLeaf === 'object' &&
          Schema.containsUnknownCase((decodedLeaf as Record<string, unknown>)[discriminator])
        if (!scalarLeaf && !excessKey && !unknownMember) throw new Error('unknown declaration must name an enum leaf, excess key, or genuinely unknown union member')
        if (paths.some((other, at) => at !== index && (other.pointer === path.pointer || other.pointer.startsWith(`${path.pointer}/`)))) {
          throw new Error('unknown paths must be unique and nonoverlapping')
        }
      }
      const clean = paths.reduce((input, path) => patch(input, path.pointer, path.known_value), value)
      const cleanError = decode(clean, 'strict')
      if (cleanError !== undefined) failure(pointer, `undeclared contamination or invalid known_value: ${cleanError}`)
      for (const path of paths) {
        // Keep this path's original value, restore every other declared path. This probes
        // each declared path independently even when the codec only reports a root error.
        const isolated = paths.filter((other) => other !== path).reduce((input, other) => patch(input, other.pointer, other.known_value), value)
        if (decode(isolated, 'strict') === undefined) failure(path.pointer, 'declared path does not fail strict decoding')
      }
    } catch (error) {
      failure(pointer, error instanceof Error ? error.message : String(error))
    }
  }
  for (const path of declarations) if (!covered.has(path.pointer)) failure(path.pointer, 'unknown pointer is not inside a wire value')
  return failures
}

/** Every variant of every slice of `world`. */
export const everyVariant = (world: World): AnySlice[] =>
  SLICE_KINDS.flatMap((kind) => world.available[kind].map((variant) => world.with({ [kind]: variant }).slices[kind] as AnySlice))

/**
 * Scale worlds whose variants are decoded at the first `now` only. Later instants decode their
 * default slices, so the relative-time path still runs on scale-sized data while the CI lane stays
 * bounded. Every other world decodes every variant at every instant.
 */
export const FIRST_INSTANT_VARIANTS: ReadonlySet<string> = new Set(['huge'])

export const decodeCatalog = (nows: readonly number[]): DecodeFailure[] => {
  const failures: DecodeFailure[] = []
  for (const [index, now] of nows.entries()) for (const { id } of catalog) {
    const world = loadWorld(id, { now })
    if (index > 0 && FIRST_INSTANT_VARIANTS.has(id)) {
      for (const kind of SLICE_KINDS) failures.push(...decodeSlice(id, world.slices[kind]))
      continue
    }
    // Decode one variant at a time: huge's per-agent variants must not all stay resident.
    for (const kind of SLICE_KINDS) for (const variant of world.available[kind]) {
      const slice = world.with({ [kind]: variant }).slices[kind]
      failures.push(...decodeSlice(id, slice))
    }
  }
  return failures
}

const isSliceKind = (value: string): value is SliceKind => (SLICE_KINDS as readonly string[]).includes(value)

/** Strict-decodes the wire values of one committed slice file as it is on disk. */
const decodeFile = (world: string, path: string): DecodeFailure[] => {
  const failure = (slice: string, variant: string, message: string): DecodeFailure[] => [{ world, slice, variant, pointer: '', message }]
  const file = JSON.parse(readFileSync(path, 'utf8')) as SliceFile
  if (file.format !== SLICE_FORMAT || !isSliceKind(file.slice)) return failure(String(file.slice), String(file.variant), `not a ${SLICE_FORMAT} file`)
  // The file's state and timeline are untrusted here; the strict codecs below are what validate them.
  const slice = { kind: file.slice, variant: file.variant, source: file.source, decode: file.decode, unknown: file.unknown, loading: file.loading,
    state: file.state, timeline: file.timeline } as AnySlice
  try {
    return decodeSlice(world, slice)
  } catch (error) {
    return failure(file.slice, file.variant, `malformed slice: ${error instanceof Error ? error.message : String(error)}`)
  }
}

/** Every `<world>/<slice>.json` under `dir`; `_`-prefixed directories hold non-slice data. */
export const decodeFixtures = (dir: string): DecodeFailure[] =>
  readdirSync(dir, { withFileTypes: true })
    .filter((entry) => entry.isDirectory() && !entry.name.startsWith('_'))
    .sort((a, b) => a.name.localeCompare(b.name))
    .flatMap((entry) => readdirSync(join(dir, entry.name)).filter((name) => name.endsWith('.json')).sort()
      .flatMap((name) => decodeFile(entry.name, join(dir, entry.name, name))))

if (import.meta.main) {
  const random = ANCHOR_MS + Math.round((Math.random() - 0.5) * 2 * 400 * 365 * 86_400_000)
  const failures = [...decodeCatalog([ANCHOR_MS, random]), ...decodeFixtures(FIXTURES_DIR)]
  for (const failure of failures) {
    console.error(`${failure.world}/${failure.slice}:${failure.variant} ${failure.pointer}\n  ${failure.message.split('\n').join('\n  ')}`)
  }
  console.log(`decode: ${failures.length === 0 ? 'ok' : `${failures.length} failure(s)`} (now = anchor and ${new Date(random).toISOString()})`)
  process.exitCode = failures.length === 0 ? 0 : 1
}
