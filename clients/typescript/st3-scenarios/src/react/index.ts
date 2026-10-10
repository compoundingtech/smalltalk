import * as React from 'react'

import { foldSlice, manualClock, syncStatusAt, type Clock, type SliceKind, type SliceStates, type SyncStatus, type SyncSurface, type World } from '../index.ts'

export type WireSlice<TKind extends SliceKind> = {
  readonly variant: string
  readonly loading: boolean
  readonly state: SliceStates[TKind]
} & (TKind extends 'sync' ? { readonly status: Record<SyncSurface, SyncStatus> } : {})

/** Replay can supply reads without introducing a transport dependency into React. */
export interface ServedSlices {
  readonly served: () => ReadonlySet<SliceKind>
}

export interface ReadTracker {
  readonly reads: () => ReadonlySet<SliceKind>
  readonly record: (kind: SliceKind) => void
}

export const createReadTracker = (source?: ServedSlices): ReadTracker => {
  const recorded = new Set<SliceKind>()
  return {
    reads: () => new Set([...recorded, ...(source?.served() ?? [])]),
    record: (kind) => { recorded.add(kind) },
  }
}

export interface ScenarioProviderProps {
  readonly world: World
  readonly clock?: Clock
  readonly tracker?: ReadTracker | ServedSlices
  readonly children?: React.ReactNode
}

interface ScenarioContextValue {
  readonly world: World
  readonly atMs: number
  readonly tracker: ReadTracker
}

const ScenarioContext = React.createContext<ScenarioContextValue | undefined>(undefined)

export const ScenarioProvider = ({ world, clock: suppliedClock, tracker: suppliedTracker, children }: ScenarioProviderProps): React.ReactElement => {
  const clock = React.useMemo(() => suppliedClock ?? manualClock(world.now), [suppliedClock, world.now])
  const tracker = React.useMemo(() => suppliedTracker !== undefined && 'record' in suppliedTracker
    ? suppliedTracker
    : createReadTracker(suppliedTracker), [suppliedTracker])
  const subscribe = React.useCallback((listener: () => void) => {
    const unsubscribe = clock.subscribe(listener)
    // Real clocks notify on scheduled work, not continuously. Own the world's future wakeups.
    const offsets = new Set(Object.values(world.slices).flatMap((slice) => slice.timeline.map((event) => event.at_ms)))
    world.slices.sync.state.expected.forEach(({ at_ms }) => offsets.add(at_ms))
    const cancel = [...offsets].filter((at) => world.now + at > clock.now()).map((at) => clock.schedule(world.now + at, () => {}))
    return () => { unsubscribe(); cancel.forEach((stop) => stop()) }
  }, [clock, world])
  const now = React.useSyncExternalStore(subscribe, clock.now, clock.now)
  const value = React.useMemo(() => ({ world, atMs: now - world.now, tracker }), [world, now, tracker])
  return React.createElement(ScenarioContext.Provider, { value }, children)
}

export function useScenarioSlice<TKind extends SliceKind>(kind: TKind): WireSlice<TKind>
export function useScenarioSlice<TKind extends SliceKind, TResult>(kind: TKind, project: (slice: WireSlice<TKind>) => TResult): TResult
export function useScenarioSlice<TKind extends SliceKind, TResult>(kind: TKind, project?: (slice: WireSlice<TKind>) => TResult): WireSlice<TKind> | TResult {
  const context = React.useContext(ScenarioContext)
  if (context === undefined) throw new Error('useScenarioSlice requires ScenarioProvider')
  const { world, atMs, tracker } = context
  tracker.record(kind)
  const slice = world.slices[kind]
  const wire = React.useMemo(() => ({
    variant: slice.variant,
    loading: slice.loading,
    state: foldSlice(slice, atMs).state,
    ...(kind === 'sync' ? { status: syncStatusAt(world.slices.sync, atMs, world.now) } : {}),
  }) as WireSlice<TKind>, [slice, atMs, kind, world.now, world.slices.sync])
  return React.useMemo(() => project === undefined ? wire : project(wire), [wire, project])
}
