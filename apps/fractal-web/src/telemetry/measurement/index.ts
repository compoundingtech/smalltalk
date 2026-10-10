import { mountMeasurementHud, recordCommit } from './developer.ts'
export { counters, createCounters, getDebug, incrDebug, incrDebugRuntime, setDebug } from './counters.ts'
export type { Counters, DebugBag } from './counters.ts'
export type { MeasurementResult, FrameClock } from './engine.ts'

/** Vite/Buck must define PERF explicitly; never infer perf policy from a stored preference. */
export const developmentMeasurements = import.meta.env.DEV || import.meta.env.PERF
  ? { mountMeasurementHud, recordCommit }
  : undefined
