import * as React from 'react'
import type { HumanTimeZone } from './human-time'

/** Clock context for leaf time labels; free of styling so data providers can install it. */

/** Epoch milliseconds at which a subscriber's output can next change; `undefined` needs no timed wake. */
export type ClockDue = () => number | undefined

/** A subscribable clock. Leaves read it directly so a tick re-renders only labels whose text changed. */
export interface HumanClock {
  readonly now: () => number
  /** Deadline clocks wake at `due`; externally driven fixture clocks may ignore it. */
  readonly subscribe: (onTick: () => void, due?: ClockDue) => () => void
}

/** The host page's timer and visibility, injectable so the scheduler is testable without a browser. */
export interface ClockHost {
  readonly now: () => number
  readonly setTimer: (run: () => void, ms: number) => unknown
  readonly clearTimer: (timer: unknown) => void
  readonly hidden: () => boolean
  readonly onVisibilityChange: (listener: () => void) => () => void
}

const browserHost = (): ClockHost => ({
  now: () => Date.now(),
  setTimer: (run, ms) => setTimeout(run, ms),
  clearTimer: timer => clearTimeout(timer as Parameters<typeof clearTimeout>[0]),
  hidden: () => typeof document !== 'undefined' && document.visibilityState === 'hidden',
  onVisibilityChange: listener => {
    if (typeof document === 'undefined') return () => {}
    document.addEventListener('visibilitychange', listener)
    return () => document.removeEventListener('visibilitychange', listener)
  },
})

const maxTimerMs = 2_147_483_647

/**
 * One shared deadline timer: wake only due subscribers, and arm nothing while hidden.
 * Becoming visible wakes every subscriber once instead of replaying missed ticks.
 */
export const createSystemClock = (host: ClockHost = browserHost()): HumanClock => {
  const subscribers = new Map<() => void, ClockDue | undefined>()
  let timer: unknown
  let armedAt: number | undefined
  let stopVisibility: (() => void) | undefined
  const disarm = () => {
    if (timer !== undefined) host.clearTimer(timer)
    timer = undefined
    armedAt = undefined
  }
  const arm = () => {
    if (host.hidden()) { disarm(); return }
    let next = Infinity
    for (const due of subscribers.values()) {
      const at = due?.()
      if (at !== undefined && Number.isFinite(at) && at < next) next = at
    }
    if (next === Infinity) { disarm(); return }
    if (armedAt === next && timer !== undefined) return
    disarm()
    armedAt = next
    timer = host.setTimer(fire, Math.min(maxTimerMs, Math.max(1, next - host.now())))
  }
  const fire = () => {
    timer = undefined
    armedAt = undefined
    const now = host.now()
    const ready: (() => void)[] = []
    for (const [onTick, due] of subscribers) {
      const at = due?.()
      if (at !== undefined && at <= now) ready.push(onTick)
    }
    for (const onTick of ready) onTick()
    arm()
  }
  const visibility = () => {
    if (host.hidden()) { disarm(); return }
    for (const onTick of [...subscribers.keys()]) onTick()
    arm()
  }
  return {
    now: host.now,
    subscribe: (onTick, due) => {
      subscribers.set(onTick, due)
      stopVisibility ??= host.onVisibilityChange(visibility)
      arm()
      return () => {
        subscribers.delete(onTick)
        if (subscribers.size > 0) { arm(); return }
        disarm()
        stopVisibility?.()
        stopVisibility = undefined
      }
    },
  }
}

export const systemClock: HumanClock = createSystemClock()
/** A clock that never ticks, for stories, tests and deterministic screenshots. */
export const fixedClock = (now: number): HumanClock => ({ now: () => now, subscribe: () => () => {} })

const ClockContext = React.createContext<HumanClock>(systemClock)
const ZoneContext = React.createContext<HumanTimeZone>({})

export const HumanClockProvider = ({ clock, zone, children }: { readonly clock: HumanClock; readonly zone?: HumanTimeZone; readonly children?: React.ReactNode }) => {
  const outerZone = React.useContext(ZoneContext)
  return <ClockContext.Provider value={clock}><ZoneContext.Provider value={zone ?? outerZone}>{children}</ZoneContext.Provider></ClockContext.Provider>
}

export const useHumanTimeZone = (): HumanTimeZone => React.useContext(ZoneContext)

/** Wake only when the projected label can change; React skips wakes that retain the text. */
export const useClockLabel = (label: (now: number, zone: HumanTimeZone) => string, next?: (now: number, zone: HumanTimeZone) => number | undefined): string => {
  const clock = React.useContext(ClockContext)
  const zone = React.useContext(ZoneContext)
  const nextRef = React.useRef(next)
  nextRef.current = next
  const dueAt = next?.(clock.now(), zone)
  const subscribe = React.useCallback((onTick: () => void) => {
    let pending = nextRef.current?.(clock.now(), zone)
    return clock.subscribe(() => { pending = nextRef.current?.(clock.now(), zone); onTick() }, () => pending)
  }, [clock, zone, dueAt])
  return React.useSyncExternalStore(subscribe, () => label(clock.now(), zone), () => label(clock.now(), zone))
}

/** Read time without subscribing; pair with useClockWake for time-dependent output. */
export const useClockTime = (): number => React.useContext(ClockContext).now()

/** Wake once at `at`, and once on visibility recovery; never replay missed updates. */
export const useClockWake = (at: number | undefined): void => {
  const clock = React.useContext(ClockContext)
  const [, wake] = React.useReducer((count: number) => count + 1, 0)
  React.useEffect(() => {
    let fired = at === undefined
    return clock.subscribe(() => { if (at !== undefined && clock.now() >= at) fired = true; wake() }, () => fired ? undefined : at)
  }, [clock, at])
}
