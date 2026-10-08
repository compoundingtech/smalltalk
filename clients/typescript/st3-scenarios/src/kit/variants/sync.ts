import type { FactoryContext } from '../context.ts'
import * as resources from '../resources.ts'
import type { Slice, SyncExpectation, SyncSurface } from '../slice.ts'
import type { SyncStatus } from '../syncStatus.ts'
import type { Slices, VariantTable } from '../world.ts'

/** Socket-backed surfaces of a world: the agent window and every conversation it shows. */
export const socketSurfaces = (slices: Pick<Slices, 'conversation'>): SyncSurface[] => [
  'agents',
  ...slices.conversation.state.threads.map((thread) => `conversation:${thread.agent}`),
]

const everySurface = (surfaces: SyncSurface[], at_ms: number, status: SyncStatus, compare: SyncExpectation['compare']): SyncExpectation[] =>
  surfaces.map((surface) => ({ surface, at_ms, status, compare }))

/** Healthy transport: every surface goes `Live` from its first snapshot or conversation frame. */
export const liveSync = (ctx: FactoryContext, slices: Pick<Slices, 'conversation'>): Slice<'sync'> => ({
  kind: 'sync',
  variant: 'live',
  source: { _tag: 'synthetic', seed: 0 },
  decode: 'strict',
  loading: false,
  state: {
    capabilities: resources.capabilities(ctx),
    expected: everySurface(socketSurfaces(slices), 0, { _tag: 'Live', since: 0 }, 'shape'),
  },
  timeline: [],
})

/** First socket drop after `Live`. */
const DROP_MS = 4_000
const reconnecting = (lastLiveAt: number): SyncStatus => ({
  _tag: 'Stale',
  reason: { _tag: 'Reconnecting', attempt: 1, nextAt: DROP_MS, issue: 'the collections socket ended' },
  lastLiveAt,
})

export const syncVariants: VariantTable['sync'] = {
  live: (ctx, base) => liveSync(ctx, base),
  'socket-dropped': (ctx, base) => {
    const surfaces = socketSurfaces(base)
    return {
      ...liveSync(ctx, base),
      variant: 'socket-dropped',
      state: {
        capabilities: resources.capabilities(ctx),
        expected: [
          ...everySurface(surfaces, 0, { _tag: 'Live', since: 0 }, 'shape'),
          ...everySurface(surfaces, DROP_MS, reconnecting(0), 'shape'),
        ],
      },
      timeline: [{ _tag: 'close', at_ms: DROP_MS, store: 0, code: 1006, reason: '' }],
    }
  },
  reconnected: (ctx, base) => {
    const surfaces = socketSurfaces(base)
    return {
      ...liveSync(ctx, base),
      variant: 'reconnected',
      state: {
        capabilities: resources.capabilities(ctx),
        expected: [
          ...everySurface(surfaces, 0, { _tag: 'Live', since: 0 }, 'shape'),
          ...everySurface(surfaces, DROP_MS, reconnecting(0), 'shape'),
          ...everySurface(surfaces, DROP_MS + 6_000, { _tag: 'Live', since: DROP_MS + 6_000 }, 'shape'),
        ],
      },
      timeline: [
        { _tag: 'close', at_ms: DROP_MS, store: 0, code: 1006, reason: '' },
        { _tag: 'reopen', at_ms: DROP_MS, store: 0, after_ms: 3_000 },
      ],
    }
  },
  'subscription-limit-local': (ctx, base) => ({
    ...liveSync(ctx, base),
    variant: 'subscription-limit-local',
    state: {
      capabilities: resources.capabilities(ctx, { collectionsV1: false }),
      local: { _tag: 'visible-follows', count: 9 },
      expected: [
        {
          surface: 'follow:9',
          at_ms: 0,
          status: { _tag: 'Failed', cause: { _tag: 'Local', kind: 'subscription-limit', detail: { cap: 8 } } },
          compare: 'exact',
        },
      ],
    },
  }),
}
