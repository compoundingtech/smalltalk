/**
 * Follow admission: which follows hold one of the collections socket's subscription slots.
 *
 * Pure bookkeeping, no I/O. One entry per follow key. Visibility is recorded per key even before
 * the follow opens (the UI marks a pane visible on mount, then reads its data), and a key never
 * seen defaults to visible: opening a follow means something wants it now.
 *
 * Slots live in one pool by default. With `conversationSlots` set, conversation follows own a
 * dedicated lane of that size and every other follow (windows, terminals) owns the rest, so
 * neither side can starve the other. Within a lane:
 *
 * - Opening below the lane cap admits.
 * - Opening at the cap evicts the least-recently-visible invisible follow of that lane.
 * - Visible follows are never evicted; with none invisible the open is refused (`Full`).
 * - A server subscription limit lowers the refused lane's cap to what that lane actually held
 *   without the refused key, then runs the same eviction for the refused follow. The server's
 *   cap is global, so repeated refusals converge each lane downward independently.
 */

/** The outcome of asking for a slot. `evict` names the follow that must give its slot up first. */
export type AdmissionResult<K> =
  | { readonly _tag: 'Admitted'; readonly evict: K | undefined }
  | { readonly _tag: 'Full' }

/** Which slot lane a follow admits into; only conversation follows get a dedicated budget. */
export type FollowLane = 'conversation' | 'shared'

interface Visibility {
  readonly visible: boolean
  /** Logical time the follow was last visible; the smallest invisible one is evicted first. */
  readonly lastVisible: number
}

/** The admission table for one socket. */
export interface Admission<K> {
  /** Effective total cap across lanes: the configured one until a server limit lowers a lane. */
  readonly cap: () => number
  readonly laneCap: (lane: FollowLane) => number
  readonly laneSplit: () => boolean
  /** Every admitted key across lanes, whatever its visibility. */
  readonly admitted: () => ReadonlySet<K>
  readonly isVisible: (key: K) => boolean
  /** Ask for a slot; on `Admitted` with `evict`, the caller must end that follow. */
  readonly open: (key: K, lane?: FollowLane) => AdmissionResult<K>
  /** Record visibility; invisible follows keep their slot until an open needs it. */
  readonly setVisible: (args: { readonly key: K; readonly visible: boolean }) => void
  /** The follow ended (finalized or evicted): its slot is free. Visibility is kept for a reopen. */
  readonly close: (key: K) => void
  /** Resize in place, preserving visibility; return LRU-invisible keys removed from the lane.
   * Visible over-cap follows stay until they end or become invisible; new opens are refused.
   */
  readonly resizeLane: (lane: FollowLane, cap: number) => readonly K[]
  /**
   * The server refused `key` for its own subscription limit: the lane's cap becomes what the
   * lane held without it, then `key` asks again.
   */
  readonly serverLimit: (key: K, lane?: FollowLane) => AdmissionResult<K>
}

/** A fresh table with `cap` slots, optionally split into a dedicated conversation lane. */
export const makeAdmission = <K>({
  cap: initialCap,
  conversationSlots,
}: {
  readonly cap: number
  readonly conversationSlots?: number
}): Admission<K> => {
  if (!Number.isSafeInteger(initialCap) || initialCap < 1) {
    throw new RangeError(`maxFollows must be a positive integer, got ${initialCap}`)
  }
  const lanes =
    conversationSlots === undefined
      ? undefined
      : (() => {
          if (
            !Number.isSafeInteger(conversationSlots) ||
            conversationSlots < 1 ||
            conversationSlots >= initialCap
          ) {
            throw new RangeError(
              `conversationSlots must be a positive integer below maxFollows (${initialCap}), got ${conversationSlots}`,
            )
          }
          return { conversation: conversationSlots, shared: initialCap - conversationSlots }
        })()
  let singleCap = initialCap
  let conversationCap = lanes?.conversation ?? 0
  let sharedCap = lanes === undefined ? initialCap : lanes.shared
  let clock = 0
  const single = new Set<K>()
  const conversations = new Set<K>()
  const shared = new Set<K>()
  const laneSets = lanes === undefined ? undefined : { conversation: conversations, shared }
  const visibility = new Map<K, Visibility>()

  const isVisible = (key: K) => visibility.get(key)?.visible ?? true

  const setFor = (lane: FollowLane): Set<K> =>
    laneSets === undefined ? single : laneSets[lane]

  const capFor = (lane: FollowLane): number =>
    laneSets === undefined ? singleCap : lane === 'conversation' ? conversationCap : sharedCap

  const setCapFor = (lane: FollowLane, value: number) => {
    if (laneSets === undefined) singleCap = value
    else if (lane === 'conversation') conversationCap = value
    else sharedCap = value
  }

  const leastRecentlyVisible = (except: K | undefined, pool: Set<K>): K | undefined => {
    let candidate: K | undefined
    let oldest = Number.POSITIVE_INFINITY
    for (const key of pool) {
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

  const resizeLane = (lane: FollowLane, cap: number): readonly K[] => {
    if (!Number.isSafeInteger(cap) || cap < 1)
      throw new RangeError(`lane cap must be a positive integer, got ${cap}`)
    setCapFor(lane, cap)
    const pool = setFor(lane)
    const evicted: K[] = []
    while (pool.size > cap) {
      const key = leastRecentlyVisible(undefined, pool)
      if (key === undefined) break
      pool.delete(key)
      evicted.push(key)
    }
    return evicted
  }

  const openIn = (key: K, lane: FollowLane): AdmissionResult<K> => {
    const pool = setFor(lane)
    if (pool.has(key)) return { _tag: 'Admitted', evict: undefined }
    if (pool.size > capFor(lane)) return { _tag: 'Full' }
    if (pool.size < capFor(lane)) {
      pool.add(key)
      return { _tag: 'Admitted', evict: undefined }
    }
    const evict = leastRecentlyVisible(key, pool)
    if (evict === undefined) return { _tag: 'Full' }
    pool.delete(evict)
    pool.add(key)
    return { _tag: 'Admitted', evict }
  }

  return {
    cap: () => (laneSets === undefined ? singleCap : conversationCap + sharedCap),
    laneCap: capFor,
    laneSplit: () => laneSets !== undefined,
    resizeLane,
    admitted: () =>
      laneSets === undefined ? single : (new Set([...conversations, ...shared]) as ReadonlySet<K>),
    isVisible,
    open: (key, lane = 'shared') => openIn(key, lane),
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
      single.delete(key)
      conversations.delete(key)
      shared.delete(key)
    },
    serverLimit: (key, lane = 'shared') => {
      const pool = setFor(lane)
      pool.delete(key)
      setCapFor(lane, Math.min(capFor(lane), Math.max(1, pool.size)))
      return openIn(key, lane)
    },
  }
}
