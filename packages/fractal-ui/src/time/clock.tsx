import * as React from 'react'
import type { HumanTimeZone } from './human-time'

/** Clock context for leaf time labels; free of styling so data providers can install it. */

/** A subscribable clock. Leaves read it directly so a tick re-renders only labels whose text changed. */
export interface HumanClock {
  readonly now: () => number
  readonly subscribe: (onTick: () => void) => () => void
}

const listeners = new Set<() => void>()
let systemTimer: ReturnType<typeof setInterval> | undefined
/** Wall clock ticking once a second while at least one label is mounted. */
export const systemClock: HumanClock = {
  now: () => Date.now(),
  subscribe: (onTick) => {
    listeners.add(onTick)
    systemTimer ??= setInterval(() => { for (const listener of listeners) listener() }, 1_000)
    return () => {
      listeners.delete(onTick)
      if (listeners.size === 0 && systemTimer !== undefined) { clearInterval(systemTimer); systemTimer = undefined }
    }
  },
}
/** A clock that never ticks, for stories, tests and deterministic screenshots. */
export const fixedClock = (now: number): HumanClock => ({ now: () => now, subscribe: () => () => {} })

const ClockContext = React.createContext<HumanClock>(systemClock)
const ZoneContext = React.createContext<HumanTimeZone>({})

/** Supplies the clock (and optionally a pinned locale/zone) to every time label below it. */
export const HumanClockProvider = ({ clock, zone, children }: { readonly clock: HumanClock; readonly zone?: HumanTimeZone; readonly children?: React.ReactNode }) => {
  const outerZone = React.useContext(ZoneContext)
  return <ClockContext.Provider value={clock}><ZoneContext.Provider value={zone ?? outerZone}>{children}</ZoneContext.Provider></ClockContext.Provider>
}

/** The viewer zone from context; pure formatters take it explicitly. */
export const useHumanTimeZone = (): HumanTimeZone => React.useContext(ZoneContext)
const noTicks = () => () => {}

/**
 * Subscribe to the context clock through a label projection. React compares the returned
 * strings, so a once-a-second tick re-renders only labels whose text actually changed.
 */
export const useClockLabel = (label: (now: number, zone: HumanTimeZone) => string, fixedNow?: number): string => {
  const clock = React.useContext(ClockContext)
  const zone = React.useContext(ZoneContext)
  const snapshot = () => label(fixedNow ?? clock.now(), zone)
  return React.useSyncExternalStore(fixedNow === undefined ? clock.subscribe : noTicks, snapshot, snapshot)
}
