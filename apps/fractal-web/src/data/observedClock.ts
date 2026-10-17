import { Option } from 'effect'
import * as Atom from 'effect/reactivity/Atom'
import type { DataSource } from './source.ts'

/** Presentation time is distinct from wall time and from an SDK data freshness verdict. */
export type ObservedClock =
  | { readonly _tag: 'Live'; readonly now: number }
  | { readonly _tag: 'Paused'; readonly now: number }

/** Retain the last live clock sample while transport is unavailable. Recovery resumes wall time.
 * Atom self-retention shares one clock across rows and retained panes, without a React subscription. */
export const createObservedClock = (source: Pick<DataSource, 'now' | 'clock' | 'connection' | 'network'> & {
  readonly sync?: Pick<NonNullable<DataSource['sync']>, 'gateway'>
}): Atom.Atom<ObservedClock> =>
  Atom.make((get): ObservedClock => {
    const connection = get(source.connection)
    const network = source.network === undefined ? undefined : get(source.network)
    const sync = source.sync === undefined ? undefined : get(source.sync.gateway)
    const paused = network?._tag === 'Offline' || connection._tag !== 'Live' || (sync !== undefined && sync.status._tag !== 'Live')
    if (paused) {
      const previous = Option.getOrUndefined(get.self<ObservedClock>())
      // A live source's `now` atom is minute-coarse, so the pause instant is read from its fresh clock.
      return previous?._tag === 'Paused' ? previous : { _tag: 'Paused', now: source.clock?.now() ?? previous?.now ?? get(source.now) }
    }
    return { _tag: 'Live', now: get(source.now) }
  }).pipe(Atom.keepAlive)
