import { drawCast, type Cast, type CastSpec } from './cast.ts'
import { type FactoryContext } from './context.ts'
import { fork, rngFromSeed } from './rng.ts'
import { SLICE_KINDS, type AnySlice, type Slice, type SliceKind, type TimelineEvent } from './slice.ts'
import { timeContext } from './time.ts'

export type Slices = { readonly [K in SliceKind]: Slice<K> }

/** Builds one variant of slice `K` over the world's cast. `base` holds the world's own slices. */
export type VariantBuilder<K extends SliceKind> = (ctx: FactoryContext, base: Slices) => Slice<K>
export type VariantTable = { readonly [K in SliceKind]: Readonly<Record<string, VariantBuilder<K>>> }

export interface WorldDefinition {
  readonly id: string
  readonly title: string
  readonly narrative: string
  readonly seed: number
  readonly cast: CastSpec
  /** The world's own slices. */
  readonly slices: (ctx: FactoryContext) => Slices
  /** World-specific variants; they win over the kit's generic ones of the same name. */
  readonly variants?: Partial<{ readonly [K in SliceKind]: Readonly<Record<string, VariantBuilder<K>>> }>
  /** Namespace of generated ids; a derived world keeps its base world's ids. Defaults to `id`. */
  readonly scope?: string
  /** Variants that replace the world's own slices (worlds derived from another world). */
  readonly selected?: Partial<Record<SliceKind, string>>
}

export type SliceOverride<K extends SliceKind> = string | Slice<K>
export type WorldOverrides = Partial<{ readonly [K in SliceKind]: SliceOverride<K> }>

export interface World {
  readonly id: string
  readonly title: string
  readonly narrative: string
  readonly seed: number
  /** Epoch ms that offsets are relative to. */
  readonly now: number
  readonly cast: Cast
  readonly slices: Slices
  /** The variant name of each slice (`custom` for a slice value passed to `with`). */
  readonly variants: Readonly<Record<SliceKind, string>>
  /** Variant names available per slice. */
  readonly available: Readonly<Record<SliceKind, readonly string[]>>
  /** Same cast and clock, the given slices replaced. */
  readonly with: (overrides: WorldOverrides) => World
}

export class UnknownVariantError extends Error {}

/**
 * Store versions (spec "Store versions"): one store per world starting at 1; every state-changing
 * event advances it by one, in time order across slices. Screens and sync events change no state.
 */
const changesState = (event: TimelineEvent): boolean => {
  switch (event._tag) {
    case 'changes':
    case 'entries':
    case 'replace':
    case 'incarnation':
    case 'end':
      return true
    default:
      return false
  }
}

const assignStores = (slices: Slices): Slices => {
  const events = SLICE_KINDS.flatMap((kind) => slices[kind].timeline.map((event, index) => ({ kind, index, event: event as TimelineEvent })))
  events.sort((a, b) => a.event.at_ms - b.event.at_ms || SLICE_KINDS.indexOf(a.kind) - SLICE_KINDS.indexOf(b.kind) || a.index - b.index)
  let store = 1
  const stores = new Map<string, number>()
  for (const { kind, index, event } of events) {
    if (changesState(event)) store += 1
    stores.set(`${kind}/${index}`, store)
  }
  const out = {} as Record<SliceKind, AnySlice>
  for (const kind of SLICE_KINDS) {
    const slice = slices[kind] as AnySlice
    out[kind] = { ...slice, timeline: slice.timeline.map((event, index) => ({ ...event, store: stores.get(`${kind}/${index}`)! })) } as AnySlice
  }
  return out as unknown as Slices
}

/** Builds a world at `now` (epoch ms). Generation is pure: same seed and `now`, same values. */
export const buildWorld = (definition: WorldDefinition, generic: VariantTable, now: number, overrides: WorldOverrides = {}): World => {
  const rng = rngFromSeed(definition.seed)
  const cast = drawCast(fork(rng, 'cast'), definition.cast)
  const ctx: FactoryContext = { world: definition.scope ?? definition.id, rng: fork(rng, 'slices'), cast, t: timeContext(now) }
  const own = definition.slices(ctx)
  const table = (kind: SliceKind): Record<string, VariantBuilder<SliceKind>> => ({
    ...(generic[kind] as Record<string, VariantBuilder<SliceKind>>),
    ...((definition.variants?.[kind] ?? {}) as Record<string, VariantBuilder<SliceKind>>),
  })
  const slices = {} as Record<SliceKind, AnySlice>
  const variants = {} as Record<SliceKind, string>
  const available = {} as Record<SliceKind, string[]>
  for (const kind of SLICE_KINDS) {
    available[kind] = ['default', ...Object.keys(table(kind)).filter((name) => name !== 'default')]
    const choice: SliceOverride<SliceKind> = overrides[kind] ?? definition.selected?.[kind] ?? 'default'
    if (typeof choice !== 'string') {
      slices[kind] = choice as AnySlice
      variants[kind] = 'custom'
    } else if (choice === 'default' && definition.selected?.[kind] === undefined) {
      slices[kind] = own[kind] as AnySlice
      variants[kind] = 'default'
    } else {
      const name = choice === 'default' ? definition.selected![kind]! : choice
      const builder = table(kind)[name]
      if (builder === undefined) throw new UnknownVariantError(`${definition.id}: no ${kind} variant ${name}`)
      const built = builder({ ...ctx, rng: fork(ctx.rng, `variant/${kind}/${name}`) }, own)
      const source = built.source._tag === 'synthetic' ? { _tag: 'synthetic' as const, seed: definition.seed } : built.source
      slices[kind] = { ...built, variant: name, source } as AnySlice
      variants[kind] = choice === 'default' ? 'default' : name
    }
  }
  const world: World = {
    id: definition.id,
    title: definition.title,
    narrative: definition.narrative,
    seed: definition.seed,
    now,
    cast,
    slices: assignStores(slices as unknown as Slices),
    variants,
    available,
    with: (next) => buildWorld(definition, generic, now, { ...overrides, ...next }),
  }
  return world
}
