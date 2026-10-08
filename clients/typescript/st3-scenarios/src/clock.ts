/**
 * Clocks drive replay and the React provider; nothing in the kit waits on its own.
 * `manualClock` releases work only through `advance`, so unit tests are deterministic.
 */
export interface Clock {
  /** Epoch milliseconds. */
  readonly now: () => number
  /** Runs `task` once the clock reaches `at` (epoch ms); returns a cancel function. */
  readonly schedule: (at: number, task: () => void) => () => void
  /** Called after time moves (manual clock) or a scheduled task ran (real clock). */
  readonly subscribe: (listener: () => void) => () => void
}

export interface ManualClock extends Clock {
  /** Moves time forward by `ms`, running every task that falls due, in time order. */
  readonly advance: (ms: number) => void
}

interface Task {
  readonly at: number
  readonly seq: number
  readonly run: () => void
}

export const manualClock = (start: number): ManualClock => {
  let current = start
  let seq = 0
  let tasks: Task[] = []
  const listeners = new Set<() => void>()
  const notify = () => listeners.forEach((listener) => listener())
  return {
    now: () => current,
    schedule: (at, run) => {
      const task = { at, seq: seq++, run }
      tasks.push(task)
      return () => {
        tasks = tasks.filter((item) => item !== task)
      }
    },
    subscribe: (listener) => {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
    advance: (ms) => {
      const target = current + ms
      for (;;) {
        const due = tasks.filter((task) => task.at <= target).sort((a, b) => a.at - b.at || a.seq - b.seq)[0]
        if (due === undefined) break
        tasks = tasks.filter((task) => task !== due)
        current = Math.max(current, due.at)
        due.run()
      }
      current = target
      notify()
    },
  }
}

/** Wall-clock time; `offset` shifts it so a pinned `now` advances in real time. */
export const realClock = (options: { readonly start?: number } = {}): Clock => {
  const offset = options.start === undefined ? 0 : options.start - Date.now()
  const listeners = new Set<() => void>()
  const now = () => Date.now() + offset
  return {
    now,
    schedule: (at, run) => {
      const handle = setTimeout(() => {
        run()
        listeners.forEach((listener) => listener())
      }, Math.max(0, at - now()))
      return () => clearTimeout(handle)
    },
    subscribe: (listener) => {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
  }
}
