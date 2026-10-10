import * as Atom from 'effect/reactivity/Atom'

/** One keyed snapshot handed to a frame ingest. */
export interface FrameEntry<K, V> {
  readonly key: K
  readonly value: V
}

/** Latest snapshot per key, committed together once per animation frame. Delta streams must
 * accumulate their deltas before scheduling a projection; dropping a delta is never safe.
 */
export const makeFrameIngest = <K, V>({
  write,
  batch = Atom.batch,
}: {
  readonly write: (entry: FrameEntry<K, V>) => void
  /** Wraps one frame's writes; defaults to `Atom.batch` so subscribers see one commit. */
  readonly batch?: (writeAll: () => void) => void
}) => {
  let pending = new Map<K, V>()
  let frame: number | undefined

  const flush = () => {
    if (frame !== undefined) cancelAnimationFrame(frame)
    frame = undefined
    if (pending.size === 0) return
    const values = pending
    pending = new Map()
    batch(() => {
      for (const [key, value] of values) write({ key, value })
    })
  }

  return {
    accept: ({ key, value }: FrameEntry<K, V>) => {
      pending.set(key, value)
      if (frame === undefined) frame = requestAnimationFrame(flush)
    },
    flush,
    dispose: () => {
      if (frame !== undefined) cancelAnimationFrame(frame)
      frame = undefined
      pending.clear()
    },
  }
}
