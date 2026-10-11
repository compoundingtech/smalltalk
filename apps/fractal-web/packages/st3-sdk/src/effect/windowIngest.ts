import type { CollectionFrame, Resource, Snapshot } from '@smalltalk/st3-client'

/** A decoded window and its exact wire rows (the roster snapshot boundary needs both). */
export interface ProcessedWindow<A> {
  readonly items: readonly A[]
  readonly rawItems: readonly Resource[]
  readonly hasMore: boolean
  readonly snapshot: Snapshot
}

type WindowFrame = Extract<CollectionFrame, { kind: 'snapshot' | 'changes' }>

/** Wire rows are JSON values. Revision alone is insufficient: projections can change at the
 * same resource revision, and unknown fields must still reach the snapshot boundary.
 */
// oxlint-disable-next-line overeng/named-args -- Hot-path structural comparator; positional args avoid per-comparison allocation.
const sameWireValue = (left: unknown, right: unknown): boolean => {
  if (left === right) return true
  if (left === null || right === null || typeof left !== 'object' || typeof right !== 'object')
    return false
  if (Array.isArray(left))
    return (
      Array.isArray(right) &&
      left.length === right.length &&
      left.every((value, i) => sameWireValue(value, right[i]))
    )
  if (Array.isArray(right)) return false
  const a = Object.entries(left)
  const b = Object.keys(right)
  return (
    a.length === b.length &&
    a.every(
      ([key, value]) => Object.hasOwn(right, key) && sameWireValue(value, Reflect.get(right, key)),
    )
  )
}

/** Decode one row at a time, yielding to browser tasks between bounded slices. A microtask or
 * Effect fiber yield is not sufficient: neither lets input/rAF run. Frames remain ordered and
 * deltas are never dropped; reset cancels all old-generation work before a fresh snapshot.
 */
export const makeWindowIngest = <A>({
  decode,
  publish,
  onSlice,
  onRejected,
  schedule = (work) => {
    const timer = setTimeout(work, 0)
    return () => clearTimeout(timer)
  },
  now = () => performance.now(),
  sliceMs = 4,
}: {
  readonly decode: (row: Resource) => A | undefined
  readonly publish: (window: ProcessedWindow<A>) => void
  readonly onSlice?: (elapsedMs: number) => void
  readonly onRejected?: (row: Resource) => void
  readonly schedule?: (work: () => void) => () => void
  readonly now?: () => number
  readonly sliceMs?: number
}) => {
  type Entry = { readonly raw: Resource; readonly value: A | undefined }
  let rows: Map<string, Entry> | undefined
  let previous: ProcessedWindow<A> | undefined
  let frames: WindowFrame[] = []
  let offset = 0
  let current: Generator<void, void> | undefined
  let cancel: (() => void) | undefined
  let running = false
  // At most one rejected revision per currently present row, not lifetime history.
  const rejected = new Map<string, string>()
  const decodeRow = (raw: Resource): A | undefined => {
    const value = decode(raw)
    if (value !== undefined) {
      rejected.delete(raw.id)
    } else if (onRejected !== undefined && rejected.get(raw.id) !== raw.revision) {
      rejected.set(raw.id, raw.revision)
      onRejected(raw)
    }
    return value
  }

  const apply = function* (frame: WindowFrame): Generator<void, void> {
    if (frame.kind === 'changes' && rows === undefined) return
    const next = frame.kind === 'snapshot' ? new Map<string, Entry>() : rows!
    if (frame.kind === 'changes') {
      for (const id of frame.removes) {
        next.delete(id)
        yield
      }
    }
    for (const raw of frame.kind === 'snapshot' ? frame.items : frame.upserts) {
      const retained = rows?.get(raw.id)
      next.set(
        raw.id,
        retained !== undefined && sameWireValue(retained.raw, raw)
          ? retained
          : { raw, value: decodeRow(raw) },
      )
      yield
    }
    const items: A[] = []
    const rawItems: Resource[] = []
    for (const id of frame.order) {
      const entry = next.get(id)
      if (entry !== undefined) {
        rawItems.push(entry.raw)
        if (entry.value !== undefined) items.push(entry.value)
      }
      yield
    }
    rows = next
    for (const id of rejected.keys()) {
      if (!next.has(id) || !frame.order.includes(id)) rejected.delete(id)
    }
    const retainedItems =
      previous !== undefined &&
      previous.items.length === items.length &&
      previous.items.every((item, i) => item === items[i])
        ? previous.items
        : items
    previous = { items: retainedItems, rawItems, hasMore: frame.has_more, snapshot: frame.snapshot }
    publish(previous)
  }

  const run = () => {
    cancel = undefined
    running = true
    const started = now()
    try {
      while (true) {
        if (current === undefined) {
          const frame = frames[offset++]
          if (frame === undefined) {
            frames = []
            offset = 0
            return
          }
          current = apply(frame)
        }
        if (current.next().done) current = undefined
        if (now() - started >= sliceMs) {
          cancel = schedule(run)
          return
        }
      }
    } finally {
      running = false
      onSlice?.(now() - started)
    }
  }

  return {
    /** Reporting state is bounded by rejected rows in the current window. */
    get rejectedRowCount(): number { return rejected.size },
    accept: (frame: CollectionFrame): void => {
      if (frame.kind !== 'snapshot' && frame.kind !== 'changes') return
      frames.push(frame)
      if (cancel === undefined && !running) cancel = schedule(run)
    },
    reset: (): void => {
      cancel?.()
      cancel = undefined
      current = undefined
      frames = []
      offset = 0
      rows = undefined
      previous = undefined
    },
  }
}
