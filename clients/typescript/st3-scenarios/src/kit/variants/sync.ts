import type { FactoryContext } from '../context.ts'
import * as resources from '../resources.ts'
import type { Selector, Slice, SyncExpectation, SyncSurface } from '../slice.ts'
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
  connecting: (ctx, base) => ({
    ...liveSync(ctx, base), variant: 'connecting',
    state: { capabilities: resources.capabilities(ctx), expected: everySurface(socketSurfaces(base), 0, { _tag: 'Connecting', attempt: 1, since: 0 }, 'shape') },
    timeline: [{ _tag: 'open-hold', at_ms: 0, store: 0 }],
  }),
  requested: (ctx, base) => ({
    ...liveSync(ctx, base), variant: 'requested',
    state: { capabilities: resources.capabilities(ctx), expected: [{ surface: 'agents', at_ms: 0, status: { _tag: 'Requested', since: 0 }, compare: 'shape' }] },
    timeline: [{ _tag: 'hold', selector: { collection: 'agents' }, at_ms: 0, store: 0 }],
  }),
  'closed-unknown': (ctx, base) => ({
    ...liveSync(ctx, base), variant: 'closed-unknown',
    state: { capabilities: resources.capabilities(ctx), expected: everySurface(socketSurfaces(base), DROP_MS, { _tag: 'Stale', reason: { _tag: 'Unknown' }, lastLiveAt: 0 }, 'shape') },
    timeline: [{ _tag: 'close', at_ms: DROP_MS, store: 0, code: 1000, reason: 'Subscription closed' }],
  }),
  'open-fail': (ctx, base) => ({
    ...liveSync(ctx, base), variant: 'open-fail',
    state: { capabilities: resources.capabilities(ctx), expected: everySurface(socketSurfaces(base), 0, { _tag: 'Stale', reason: { _tag: 'Reconnecting', attempt: 1, nextAt: 0, issue: 'Socket open failed' } }, 'shape') },
    timeline: [{ _tag: 'open-fail', at_ms: 0, store: 0, opens: 'all' }],
  }),
  'resync-coded': (ctx, base) => resyncSlice(ctx, base, true),
  'resync-uncoded': (ctx, base) => resyncSlice(ctx, base, false),
  forbidden: (ctx, base) => httpFailure(ctx, base, 'forbidden', 'capabilities', 403, 'Access to this fleet is forbidden'),
  'non-client-response': (ctx, base) => ({
    ...liveSync(ctx, base), variant: 'non-client-response',
    state: { capabilities: resources.capabilities(ctx), expected: everySurface(socketSurfaces(base), 0, { _tag: 'Stale', reason: { _tag: 'Reconnecting', attempt: 1, nextAt: 0, issue: 'The capability probe returned HTML' } }, 'shape') },
    timeline: [{ _tag: 'http-raw', route: 'capabilities', at_ms: 0, store: 0, status: 403, content_type: 'text/html', body: '<html><body>Access denied by proxy</body></html>' }],
  }),
  'subscription-error': (ctx, base) => ({
    ...liveSync(ctx, base), variant: 'subscription-error',
    state: { capabilities: resources.capabilities(ctx), expected: [
      { surface: 'agents', at_ms: 0, status: { _tag: 'Failed', cause: { _tag: 'Server', code: 'not-found', message: 'The agent collection is unavailable' } }, compare: 'exact' },
      ...everySurface(socketSurfaces(base).filter((surface) => surface !== 'agents'), 0, { _tag: 'Live', since: 0 }, 'shape'),
    ] },
    timeline: [{ _tag: 'error', selector: { collection: 'agents' }, code: 'not-found', message: 'The agent collection is unavailable', retryable: false, repeat: true, at_ms: 0, store: 0 }],
  }),
  'subscription-limit-legacy': (ctx, base) => ({
    ...liveSync(ctx, base), variant: 'subscription-limit-legacy',
    state: { capabilities: resources.capabilities(ctx, { collectionsV1: false }), expected: everySurface(socketSurfaces(base), 0, { _tag: 'Failed', cause: { _tag: 'Unknown' } }, 'shape') },
    timeline: [{ _tag: 'error', message: 'Subscription limit reached', retryable: false, repeat: true, at_ms: 0, store: 0 }],
  }),
  'capability-absent': (ctx, base) => ({
    ...liveSync(ctx, base), variant: 'capability-absent',
    state: { capabilities: resources.capabilities(ctx, { omit: ['terminal.attach'] }), expected: [
      { surface: `terminal:${base.terminal.state.terminals[0]?.terminal ?? 'attach'}`, at_ms: 0, status: { _tag: 'Failed', cause: { _tag: 'Local', kind: 'unsupported' } }, compare: 'shape' },
    ] },
  }),
  'rate-limited': (ctx, base) => httpFailure(ctx, base, 'rate-limited', 'resources', 429, 'Too many resource list requests'),
  'cursor-gap': (ctx, base) => httpFailure(ctx, base, 'cursor-gap', 'events', 409, 'The event cursor is outside retained history'),
  'page-cursor-expired': (ctx, base) => httpFailure(ctx, base, 'page-cursor-expired', 'timeline', 410, 'The older page cursor has expired'),
  evicted: (ctx, base) => ({
    ...liveSync(ctx, base), variant: 'evicted',
    state: { capabilities: resources.capabilities(ctx, { collectionsV1: false }),
      local: { _tag: 'hidden-follow', hidden: 'follow:0', visible: Array.from({ length: 8 }, (_, index) => `follow:${index + 1}`) },
      expected: [{ surface: 'follow:0', at_ms: 0, status: { _tag: 'Stale', reason: { _tag: 'Evicted' } }, compare: 'exact' }],
    },
  }),
  'progress-absent': (ctx, base) => ({
    ...liveSync(ctx, base), variant: 'progress-absent',
    state: { capabilities: resources.capabilities(ctx, { omit: ['sync-status.v1'] }), expected: [
      { surface: 'agents', at_ms: 0, status: { _tag: 'Requested', since: 0 }, compare: 'shape' },
      { surface: 'agents', at_ms: 6_000, status: { _tag: 'Live', since: 6_000 }, compare: 'shape' },
    ] },
    timeline: [{ _tag: 'hold', selector: { collection: 'agents' }, at_ms: 0, store: 0 }, { _tag: 'release', selector: { collection: 'agents' }, at_ms: 6_000, store: 0 }],
  }),
}

const resyncSlice = (ctx: FactoryContext, base: Slices, coded: boolean): Slice<'sync'> => {
  const agent = base.conversation.state.threads[0]?.agent
  // Empty/onboarding worlds have no conversation subscription yet, but always expose agents.
  const selector: Selector = agent === undefined ? { collection: 'agents' } : { collection: 'conversation', conversation: agent }
  const surface = agent === undefined ? 'agents' : `conversation:${agent}`
  const message = agent === undefined ? 'The collection peer is temporarily unavailable' : 'The conversation peer is temporarily unavailable'
  return {
    ...liveSync(ctx, base), variant: coded ? 'resync-coded' : 'resync-uncoded',
    state: { capabilities: resources.capabilities(ctx), expected: [
      ...everySurface(socketSurfaces(base), 0, { _tag: 'Live', since: 0 }, 'shape'),
      { surface, at_ms: DROP_MS, status: { _tag: 'Stale', reason: coded ? { _tag: 'Resync', code: 'remote-unavailable', message, attempt: 1 } : { _tag: 'Unknown' }, lastLiveAt: 0 }, compare: 'shape' },
    ] },
    timeline: [{ _tag: 'resync', selector, ...(coded ? { code: 'remote-unavailable', message } : {}), at_ms: DROP_MS, store: 0 }],
  }
}

const httpFailure = (ctx: FactoryContext, base: Slices, code: string, route: 'capabilities' | 'resources' | 'events' | 'timeline', status: number, message: string): Slice<'sync'> => ({
  ...liveSync(ctx, base), variant: code,
  state: { capabilities: resources.capabilities(ctx), expected: [
    ...(route === 'capabilities' ? [] : everySurface(socketSurfaces(base), 0, { _tag: 'Live', since: 0 }, 'shape')),
    ...everySurface(route === 'capabilities' ? socketSurfaces(base) : [`read:${route}`], 0, { _tag: 'Failed', cause: { _tag: 'Server', code, message } }, 'exact'),
  ] },
  timeline: [{ _tag: 'http-error', route, at_ms: 0, store: 0, status,
    ...(route === 'timeline' ? { when: { cursor: 'present' as const } } : {}),
    envelope: { api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', request_id: 'request/scenario-sync-error', code, message, retryable: false, details: {} },
  }],
})
