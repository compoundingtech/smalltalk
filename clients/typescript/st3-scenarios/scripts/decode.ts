/**
 * Decode gate (spec "Gates" 3): every wire value of every world and variant decodes with the real
 * client-v0 codecs in strict mode, at the anchor and rebased to another `now`, and so does every
 * committed slice file that non-TypeScript readers load.
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

/** Strict-decodes every wire value of `slice`; returns one failure per value that does not decode. */
export const decodeSlice = (world: string, slice: AnySlice): DecodeFailure[] =>
  wireValues(slice).flatMap(({ pointer, definition, value }) => {
    try {
      Schema.decodeUnknownSync(Schema[definition] as never, slice.decode)(value)
      return []
    } catch (error) {
      return [{ world, slice: slice.kind, variant: slice.variant, pointer, message: error instanceof Error ? error.message : String(error) }]
    }
  })

/** Every variant of every slice of `world`. */
export const everyVariant = (world: World): AnySlice[] =>
  SLICE_KINDS.flatMap((kind) => world.available[kind].map((variant) => world.with({ [kind]: variant }).slices[kind] as AnySlice))

export const decodeCatalog = (nows: readonly number[]): DecodeFailure[] =>
  nows.flatMap((now) =>
    catalog.flatMap(({ id }) => {
      const world = loadWorld(id, { now })
      return everyVariant(world).flatMap((slice) => decodeSlice(id, slice))
    }),
  )

const isSliceKind = (value: string): value is SliceKind => (SLICE_KINDS as readonly string[]).includes(value)

/** Strict-decodes the wire values of one committed slice file as it is on disk. */
const decodeFile = (world: string, path: string): DecodeFailure[] => {
  const failure = (slice: string, variant: string, message: string): DecodeFailure[] => [{ world, slice, variant, pointer: '', message }]
  const file = JSON.parse(readFileSync(path, 'utf8')) as SliceFile
  if (file.format !== SLICE_FORMAT || !isSliceKind(file.slice)) return failure(String(file.slice), String(file.variant), `not a ${SLICE_FORMAT} file`)
  // The file's state and timeline are untrusted here; the strict codecs below are what validate them.
  const slice = { kind: file.slice, variant: file.variant, source: file.source, decode: file.decode, loading: file.loading,
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
