/**
 * Decode gate (spec "Gates" 3): every wire value of every world and variant decodes with the real
 * client-v0 codecs in strict mode, at the anchor and rebased to another `now`.
 */
import * as Schema from '@smalltalk/st3-client/schema'

import { ANCHOR_MS, catalog, loadWorld, SLICE_KINDS, wireValues, type AnySlice, type World } from '../src/index.ts'

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

if (import.meta.main) {
  const random = ANCHOR_MS + Math.round((Math.random() - 0.5) * 2 * 400 * 365 * 86_400_000)
  const failures = decodeCatalog([ANCHOR_MS, random])
  for (const failure of failures) {
    console.error(`${failure.world}/${failure.slice}:${failure.variant} ${failure.pointer}\n  ${failure.message.split('\n').join('\n  ')}`)
  }
  console.log(`decode: ${failures.length === 0 ? 'ok' : `${failures.length} failure(s)`} (now = anchor and ${new Date(random).toISOString()})`)
  process.exitCode = failures.length === 0 ? 0 : 1
}
