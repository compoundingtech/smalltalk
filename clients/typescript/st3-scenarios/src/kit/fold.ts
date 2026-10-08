import type { Resource } from '@smalltalk/st3-client'

import type { AnySlice, Slice, SliceKind, SliceStates, SyncSurface, TimelineEvent } from './slice.ts'
import type { SyncStatus } from './syncStatus.ts'

type Keyed = { readonly id?: string }

const upsert = <A extends Keyed>(list: readonly A[], value: A): A[] => {
  const index = list.findIndex((item) => item.id === value.id)
  return index === -1 ? [...list, value] : list.map((item, at) => (at === index ? value : item))
}

const COLLECTION_OF_KIND: Record<string, string> = {
  agent: 'agents',
  runtime: 'runtimes',
  machine: 'machines',
  mission: 'missions',
  work: 'work',
  attention: 'attention',
  message: 'messages',
}

const applyChanges = <S extends object>(state: S, upserts: readonly Resource[], removes: readonly string[], order: readonly string[] | undefined): S => {
  const next = { ...state } as Record<string, unknown>
  for (const value of upserts) {
    const key = COLLECTION_OF_KIND[value.kind]
    if (key === undefined || !Array.isArray(next[key])) throw new Error(`a ${value.kind} upsert has no place in this slice`)
    next[key] = upsert(next[key] as Keyed[], value as Keyed)
  }
  for (const [key, list] of Object.entries(next)) {
    if (key !== 'order' && Array.isArray(list)) next[key] = (list as Keyed[]).filter((item) => !removes.includes(item.id ?? ''))
  }
  if ('order' in next) {
    const current = (next.order as string[]).filter((id) => !removes.includes(id))
    const added = upserts.filter((value) => value.kind === 'agent' && !current.includes(value.id)).map((value) => value.id)
    next.order = order ?? [...current, ...added]
  }
  return next as S
}

const applyEvent = (slice: AnySlice, event: TimelineEvent): AnySlice['state'] => {
  switch (event._tag) {
    case 'changes':
      return applyChanges(slice.state, event.upserts, event.removes, event.order)
    case 'entries': {
      const state = slice.state as SliceStates['conversation']
      return {
        threads: state.threads.map((thread) =>
          thread.agent === event.agent ? { ...thread, items: event.items.reduce((items, item) => upsert(items, item), thread.items) } : thread,
        ),
      }
    }
    case 'replace': {
      const state = slice.state as SliceStates['conversation']
      return {
        threads: state.threads.map((thread) =>
          thread.agent === event.agent ? { ...thread, session_id: event.session_id, items: event.items, has_more: event.has_more } : thread,
        ),
      }
    }
    case 'screen': {
      const state = slice.state as SliceStates['terminal']
      return {
        terminals: state.terminals.map((record) =>
          record.terminal === event.terminal ? { ...record, screens: [...record.screens, { at_ms: event.at_ms, screen: event.screen }] } : record,
        ),
      }
    }
    case 'incarnation': {
      const state = slice.state as SliceStates['terminal']
      return {
        terminals: state.terminals.map((record) =>
          record.terminal === event.terminal
            ? { ...record, incarnation: event.incarnation, runtime: { ...record.runtime, incarnation_id: event.incarnation } }
            : record,
        ),
      }
    }
    case 'end': {
      const state = slice.state as SliceStates['terminal']
      return {
        terminals: state.terminals.map((record) =>
          record.terminal === event.terminal ? { ...record, runtime: { ...record.runtime, state: 'exited' as const } } : record,
        ),
      }
    }
    case 'unavailable':
    case 'open-fail':
    case 'http-raw':
    case 'close':
    case 'reopen':
    case 'http-error':
    case 'http-ok':
    case 'hold':
    case 'release':
    case 'resync':
    case 'error':
    case 'notice':
    case 'notice-clear':
      return slice.state
    default: {
      const unreachable: never = event
      throw new Error(`unknown event ${JSON.stringify(unreachable)}`)
    }
  }
}

/** The slice as of clock offset `atMs`: events at or before it folded into the state. */
export const foldSlice = <K extends SliceKind>(slice: Slice<K>, atMs: number): Slice<K> => {
  let current = slice as unknown as AnySlice
  for (const event of current.timeline) {
    if (event.at_ms > atMs) break
    current = { ...current, state: applyEvent(current, event) } as AnySlice
  }
  return { ...current, timeline: current.timeline.filter((event) => event.at_ms > atMs) } as unknown as Slice<K>
}

/** The expected `SyncStatus` per surface at `atMs`, with offsets turned into instants from `now`. */
export const syncStatusAt = (slice: Slice<'sync'>, atMs: number, now: number): Record<SyncSurface, SyncStatus> => {
  const out: Record<SyncSurface, SyncStatus> = {}
  for (const expectation of slice.state.expected) {
    if (expectation.at_ms <= atMs) out[expectation.surface] = absolute(expectation.status, now)
  }
  return out
}

/** Turns the offsets of a portable status into epoch ms. */
export const absolute = (status: SyncStatus, now: number): SyncStatus => {
  switch (status._tag) {
    case 'Connecting':
    case 'Requested':
    case 'Live':
      return { ...status, since: now + status.since }
    case 'Progress':
      return { ...status, stageSince: now + status.stageSince, reportedAt: now + status.reportedAt }
    case 'Stale': {
      const reason =
        status.reason._tag === 'Reconnecting'
          ? { ...status.reason, nextAt: now + status.reason.nextAt }
          : status.reason._tag === 'Quiet'
            ? { ...status.reason, lastFrameAt: now + status.reason.lastFrameAt }
            : status.reason
      return { ...status, reason, ...(status.lastLiveAt === undefined ? {} : { lastLiveAt: now + status.lastLiveAt }) }
    }
    case 'Failed':
      return status
  }
}
