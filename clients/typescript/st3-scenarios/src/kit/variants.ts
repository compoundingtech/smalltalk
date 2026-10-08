import { child, type FactoryContext } from './context.ts'
import { terminalRun } from './factories/terminalRun.ts'
import * as resources from './resources.ts'
import type { Slice, SliceKind, SyncExpectation, SyncSurface, TerminalRecord } from './slice.ts'
import type { SyncStatus } from './syncStatus.ts'
import * as vocabulary from './vocabulary/index.ts'
import type { Slices, VariantTable } from './world.ts'

/** Generic variants every world gets; a world may replace any of them by name. */

const cleared = <K extends SliceKind>(slice: Slice<K>, state: Slice<K>['state']): Slice<K> => ({ ...slice, state, timeline: [] })

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

export const genericVariants: VariantTable = {
  roster: {
    empty: (_ctx, base) => cleared(base.roster, { agents: [], runtimes: [], machines: [], order: [] }),
    loading: (_ctx, base) => ({ ...base.roster, loading: true }),
    'one-agent': (_ctx, base) => {
      const [first] = base.roster.state.order
      const agents = base.roster.state.agents.filter((value) => value.id === first)
      const runtimes = base.roster.state.runtimes.filter((runtime) => runtime.owner_id === first)
      const hosts = new Set(runtimes.map((runtime) => runtime.owner_host_id))
      return {
        ...base.roster,
        state: {
          agents,
          runtimes,
          machines: base.roster.state.machines
            .filter((machine) => hosts.has(machine.host_id))
            .map((machine) => ({ ...machine, runtime_ids: runtimes.map((runtime) => runtime.id), occupancy: { running_runtimes: runtimes.length } })),
          order: first === undefined ? [] : [first],
        },
        timeline: base.roster.timeline.filter((event) => event.upserts.every((value) => value.id === first)),
      }
    },
  },
  details: {
    empty: (_ctx, base) => cleared(base.details, { missions: [], work: [] }),
    loading: (_ctx, base) => ({ ...base.details, loading: true }),
  },
  attention: {
    none: (_ctx, base) => cleared(base.attention, { attention: [], messages: [] }),
  },
  conversation: {
    empty: (_ctx, base) =>
      cleared(base.conversation, { threads: base.conversation.state.threads.map((thread) => ({ ...thread, items: [], has_more: false })) }),
    loading: (_ctx, base) => ({ ...base.conversation, loading: true }),
  },
  terminal: {
    none: (_ctx, base) => cleared(base.terminal, { terminals: [] }),
    running: (ctx, base) => {
      const owner = ctx.cast.agents.find((member) => member.id === base.terminal.state.terminals[0]?.owner) ?? ctx.cast.agents[0]!
      const run = terminalRun(child(ctx, 'running'), owner, {
        startedAtMs: -1_500,
        command: vocabulary.shellRuns[1].command,
        lines: vocabulary.shellRuns[1].lines,
      })
      const record = base.terminal.state.terminals[0]!
      const next: TerminalRecord = { ...record, cast: run.cast, screens: run.screens.filter((screen) => screen.at_ms <= 0) }
      return {
        ...base.terminal,
        state: { terminals: [next] },
        timeline: run.screens
          .filter((screen) => screen.at_ms > 0)
          .map((screen) => ({ _tag: 'screen' as const, at_ms: screen.at_ms, store: 0, terminal: record.terminal, screen: screen.screen })),
      }
    },
    unavailable: (_ctx, base) => {
      const record = base.terminal.state.terminals[0]!
      return { ...base.terminal, timeline: [{ _tag: 'unavailable', at_ms: 3_000, store: 0, terminal: record.terminal }] }
    },
    exited: (_ctx, base) => {
      const record = base.terminal.state.terminals[0]!
      return { ...base.terminal, timeline: [{ _tag: 'end', at_ms: 3_000, store: 0, terminal: record.terminal }] }
    },
    restarted: (_ctx, base) => {
      const record = base.terminal.state.terminals[0]!
      const incarnation = record.incarnation.replace(/:(\d+)$/, (_, n: string) => `:${Number(n) + 1}`)
      const screen = record.screens.at(-1)!.screen
      return {
        ...base.terminal,
        timeline: [
          { _tag: 'incarnation', at_ms: 3_000, store: 0, terminal: record.terminal, incarnation },
          { _tag: 'screen', at_ms: 3_500, store: 0, terminal: record.terminal, screen: { ...screen, runtime_incarnation: incarnation, revision: `${screen.revision}-r` } },
        ],
      }
    },
  },
  sync: {
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
  },
}
