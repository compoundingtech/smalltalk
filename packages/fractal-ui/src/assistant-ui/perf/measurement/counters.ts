// From compoundingtech/smalltalk#1641, commit 4f74464e0b22ea889a09d9271d076c3c6fd85df8.
export interface DebugBag { [key: string]: number }
export interface Counters {
  readonly getDebug: (key: string) => number
  readonly setDebug: (key: string, value: number) => void
  readonly incrDebug: (key: string, amount?: number) => void
  readonly snapshot: () => Readonly<DebugBag>
  readonly reset: () => void
}

/** Process-local accounting; no subscriptions, network exporters or development globals. */
export const createCounters = (): Counters => {
  const values: DebugBag = Object.create(null)
  return {
    getDebug: (key: string): number => values[key] ?? 0,
    setDebug: (key: string, value: number): void => { values[key] = value },
    incrDebug: (key: string, amount = 1): void => { values[key] = (values[key] ?? 0) + amount },
    snapshot: (): Readonly<DebugBag> => ({ ...values }),
    reset: (): void => { for (const key of Object.keys(values)) delete values[key] },
  }
}
export const counters = createCounters()
export const { getDebug, setDebug, incrDebug } = counters
