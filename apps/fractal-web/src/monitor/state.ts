/**
 * Monitor state as Effect Atoms. Reads are driven by observation: the quota poll runs only while a
 * monitor surface (panel or status item) is mounted and the page is visible, one request at a time
 * with a five-second deadline (CAG.CLI.WEB.MON-R04). History is one atom per selected ledger account, so an
 * answer for an account the reader left lands in an atom nobody renders: the selection fence of
 * CAG.CLI.TUI.TUI.MON-R16 holds by construction, independent of cancellation or arrival order.
 */
import { Cause, Effect, Queue, Stream } from 'effect'
import * as AsyncResult from 'effect/reactivity/AsyncResult'
import * as Atom from 'effect/reactivity/Atom'

import {
  initialHistory,
  initialQuota,
  projectHistory,
  reduceQuota,
  type HistoryState,
  type HistoryUnit,
  type QuotaEvent,
  type QuotaFailure,
  type QuotaState,
} from './model.ts'
import type { MonitorSource, UsageSource } from './source.ts'
import { decodeHistory, decodeQuota } from './wire.ts'

/** Fractal's default quota refresh cadence. */
export const QUOTA_INTERVAL = '30 seconds'
/** Fractal's default history refresh cadence. */
export const HISTORY_INTERVAL = '5 minutes'
/** Every read is abandoned after this and reported as a timeout (CAG.CLI.WEB.MON-R04). */
export const READ_DEADLINE = '5 seconds'
/** How long a deselected account's history stays loaded, so re-selecting it costs no read. */
export const HISTORY_IDLE_TTL = '2 minutes'

/** Resolves once the page is visible; observation follows attention (CAG.CLI.WEB-R01). */
const whenVisible: Effect.Effect<void> = Effect.callback<void>((resume) => {
  if (typeof document === 'undefined' || document.visibilityState === 'visible') {
    resume(Effect.void)
    return
  }
  const onChange = () => {
    if (document.visibilityState !== 'visible') return
    document.removeEventListener('visibilitychange', onChange)
    resume(Effect.void)
  }
  document.addEventListener('visibilitychange', onChange)
  return Effect.sync(() => document.removeEventListener('visibilitychange', onChange))
})

const bounded = <A>(read: Effect.Effect<A, QuotaFailure>): Effect.Effect<A, QuotaFailure> =>
  read.pipe(
    Effect.timeout(READ_DEADLINE),
    Effect.mapError(
      (error): QuotaFailure =>
        Cause.isTimeoutError(error)
          ? { kind: 'timeout', reason: `no answer within ${READ_DEADLINE}` }
          : error,
    ),
  )

/** One bounded quota read, as the event the reducer applies. Never fails. */
export const readQuota = (source: UsageSource): Effect.Effect<QuotaEvent> =>
  bounded(source.quota).pipe(
    Effect.flatMap((json) => {
      const decoded = decodeQuota(json, { nativeProjection: source.quotaFormat === 'native-projection' })
      return decoded._tag === 'ok'
        ? Effect.succeed<QuotaEvent>({ _tag: 'Succeeded', at: Date.now(), envelope: decoded.value })
        : Effect.fail<QuotaFailure>({ kind: 'incompatible', reason: decoded.reason })
    }),
    Effect.catch((failure) =>
      Effect.succeed<QuotaEvent>({ _tag: 'Failed', at: Date.now(), failure }),
    ),
  )

/**
 * An atom whose value is the latest state a polling loop emitted. The loop runs as a stream while
 * the atom is observed and is interrupted with it.
 */
const polled = <S>({
  initial,
  loop,
}: {
  readonly initial: S
  readonly loop: (emit: (state: S) => Effect.Effect<void>) => Effect.Effect<void>
}) =>
  Atom.make(
    () => Stream.callback<S>((queue) => loop((state) => Effect.asVoid(Queue.offer(queue, state)))),
    { initialValue: initial },
  )

/** The quota poll for one source: requested/succeeded/failed states, while observed and visible. */
export const quotaAtom = Atom.family((monitor: MonitorSource) =>
  polled({
    initial: initialQuota(monitor._tag === 'declared'),
    loop: (emit) =>
      Effect.gen(function* () {
        if (monitor._tag === 'undeclared') return
        let state = initialQuota(true)
        while (true) {
          yield* whenVisible
          state = reduceQuota({ state, event: { _tag: 'Requested', at: Date.now() } })
          yield* emit(state)
          state = reduceQuota({ state, event: yield* readQuota(monitor.source) })
          yield* emit(state)
          yield* Effect.sleep(monitor.source.cadence?.quota ?? QUOTA_INTERVAL)
        }
      }),
  }),
)

/** The latest quota state, unwrapped: the driver never fails, so the result is always a value. */
export const quotaStateAtom = Atom.family((monitor: MonitorSource) =>
  Atom.make(
    (get): QuotaState =>
      AsyncResult.getOrElse(get(quotaAtom(monitor)), () =>
        initialQuota(monitor._tag === 'declared'),
      ),
  ),
)

/** One bounded history read applied to the account's state; the last good series survives failures. */
const readHistory = ({
  source,
  ledgerAccountId,
  state,
}: {
  readonly source: UsageSource
  readonly ledgerAccountId: string
  readonly state: HistoryState
}): Effect.Effect<HistoryState> =>
  Effect.result(bounded(source.history(ledgerAccountId))).pipe(
    Effect.map((read): HistoryState => {
      if (read._tag === 'Failure')
        return { ...state, attempt: { _tag: 'failed', reason: read.failure.reason } }
      const decoded = decodeHistory(read.success)
      const series =
        decoded._tag === 'ok'
          ? projectHistory({ envelope: decoded.value, ledgerAccountId })
          : { _tag: 'error' as const, reason: decoded.reason }
      return series._tag === 'series'
        ? { last: { series: series.series, at: Date.now() }, attempt: { _tag: 'ok' } }
        : { ...state, attempt: { _tag: 'failed', reason: series.reason } }
    }),
  )

const pollHistory = ({ source, ledgerAccountId }: HistoryKey) =>
  polled({
    initial: initialHistory,
    loop: (emit) =>
      Effect.gen(function* () {
        let state: HistoryState = initialHistory
        while (true) {
          yield* whenVisible
          state = { last: state.last, attempt: { _tag: 'inFlight' } }
          yield* emit(state)
          state = yield* readHistory({ source, ledgerAccountId, state })
          yield* emit(state)
          yield* Effect.sleep(source.cadence?.history ?? HISTORY_INTERVAL)
        }
      }),
  }).pipe(Atom.setIdleTTL(HISTORY_IDLE_TTL))

/** Which account's history, read through which source. */
export interface HistoryKey {
  readonly source: UsageSource
  /** `ledgerAccountId` verbatim, as the producer published it (CAG.CLI.TUI.TUI.MON-R12). */
  readonly ledgerAccountId: string
}

/**
 * One history atom per (source, ledger account), held strongly. Not a nested `Atom.family`: the
 * outer family holds the inner family function only weakly, so after a GC the next lookup built a
 * fresh atom with a fresh loop while the idle-TTL'd old one kept polling: reads multiplied each tick
 * and the retained chart vanished (reproduced in the spike with effect 4.0.0-rc.118).
 */
const historyAtoms = new WeakMap<UsageSource, Map<string, Atom.Atom<HistoryState>>>()

/** The history atom for one account; the same key always yields the same atom and poll. */
export const historyAtom = ({ source, ledgerAccountId }: HistoryKey): Atom.Atom<HistoryState> => {
  let bySource = historyAtoms.get(source)
  if (bySource === undefined) {
    bySource = new Map()
    historyAtoms.set(source, bySource)
  }
  let atom = bySource.get(ledgerAccountId)
  if (atom === undefined) {
    const poll = pollHistory({ source, ledgerAccountId })
    atom = Atom.make((get): HistoryState => AsyncResult.getOrElse(get(poll), () => initialHistory))
    bySource.set(ledgerAccountId, atom)
  }
  return atom
}

/** Selection keyed by canonical account id; a refresh never selects on the reader's behalf. */
export const selectionAtom = Atom.family((_monitor: MonitorSource) =>
  Atom.make<string | undefined>(undefined).pipe(Atom.keepAlive),
)

const UNIT_KEY = 'wf.monitor.accountUsageHistoryUnit'
const storedUnit = (): HistoryUnit => {
  try {
    return globalThis.localStorage?.getItem(UNIT_KEY) === 'tokens' ? 'tokens' : 'usd'
  } catch {
    return 'usd'
  }
}
const unitStateAtom = Atom.make<HistoryUnit>(storedUnit()).pipe(Atom.keepAlive)

/** One global, persisted reading preference (CAG.CLI.TUI.TUI.MON-R14); not per account or selection. */
export const historyUnitAtom = Atom.writable(
  (get) => get(unitStateAtom),
  (ctx, unit: HistoryUnit) => {
    try {
      globalThis.localStorage?.setItem(UNIT_KEY, unit)
    } catch {
      // Storage may be unavailable (private mode); the preference still applies for the session.
    }
    ctx.set(unitStateAtom, unit)
  },
)
