/**
 * Follow admission: which follows hold one of the collections socket's subscription slots.
 *
 * Pure bookkeeping, no I/O. One entry per follow key. Visibility is recorded per key even before
 * the follow opens (the UI marks a pane visible on mount, then reads its data), and a key never
 * seen defaults to visible: opening a follow means something wants it now.
 *
 * - Opening below the cap admits.
 * - Opening at the cap evicts the least-recently-visible invisible follow.
 * - Visible follows are never evicted; with none invisible the open is refused (`Full`).
 * - A server subscription limit lowers the cap to what the server actually held, then runs
 *   the same eviction for the refused follow.
 */

/** The outcome of asking for a slot. `evict` names the follow that must give its slot up first. */
export type AdmissionResult<K> =
  | { readonly _tag: 'Admitted'; readonly evict: K | undefined }
  | { readonly _tag: 'Full' }

interface Visibility {
  readonly visible: boolean
  /** Logical time the follow was last visible; the smallest invisible one is evicted first. */
  readonly lastVisible: number
}

/** The admission table for one socket. */
export interface Admission<K> {
  /** Effective cap: the configured one until a server limit lowers it. */
  readonly cap: () => number
  readonly admitted: () => ReadonlySet<K>
  readonly isVisible: (key: K) => boolean
  /** Ask for a slot; on `Admitted` with `evict`, the caller must end that follow. */
  readonly open: (key: K) => AdmissionResult<K>
  /** Record visibility; invisible follows keep their slot until an open needs it. */
  readonly setVisible: (args: { readonly key: K; readonly visible: boolean }) => void
  /** The follow ended (finalized or evicted): its slot is free. Visibility is kept for a reopen. */
  readonly close: (key: K) => void
  /**
   * The server refused `key` for its own subscription limit: the cap becomes what the server
   * held without it, then `key` asks again.
   */
  readonly serverLimit: (key: K) => AdmissionResult<K>
}

/** A fresh table with `cap` slots. */
export const makeAdmission = <K>({ cap: initialCap }: { readonly cap: number }): Admission<K> => {
  if (!Number.isSafeInteger(initialCap) || initialCap < 1) {
    throw new RangeError(`maxFollows must be a positive integer, got ${initialCap}`)
  }
  let cap = initialCap
  let clock = 0
  const admitted = new Set<K>()
  const visibility = new Map<K, Visibility>()

  const isVisible = (key: K) => visibility.get(key)?.visible ?? true

  const leastRecentlyVisible = (except: K): K | undefined => {
    let candidate: K | undefined
    let oldest = Number.POSITIVE_INFINITY
    for (const key of admitted) {
      if (key === except) continue
      const entry = visibility.get(key)
      if (entry === undefined || entry.visible) continue
      if (entry.lastVisible < oldest) {
        oldest = entry.lastVisible
        candidate = key
      }
    }
    return candidate
  }

  const open = (key: K): AdmissionResult<K> => {
    if (admitted.has(key)) return { _tag: 'Admitted', evict: undefined }
    if (admitted.size < cap) {
      admitted.add(key)
      return { _tag: 'Admitted', evict: undefined }
    }
    const evict = leastRecentlyVisible(key)
    if (evict === undefined) return { _tag: 'Full' }
    admitted.delete(evict)
    admitted.add(key)
    return { _tag: 'Admitted', evict }
  }

  return {
    cap: () => cap,
    admitted: () => admitted,
    isVisible,
    open,
    setVisible: ({ key, visible }) => {
      clock += 1
      const previous = visibility.get(key)
      // An invisible follow's `lastVisible` is the moment it stopped being visible.
      visibility.set(key, {
        visible,
        lastVisible: visible || previous?.visible !== false ? clock : previous.lastVisible,
      })
    },
    close: (key) => {
      admitted.delete(key)
    },
    serverLimit: (key) => {
      admitted.delete(key)
      cap = Math.max(1, admitted.size)
      return open(key)
    },
  }
}
