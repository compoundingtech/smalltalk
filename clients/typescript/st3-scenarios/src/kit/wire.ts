import type { AnySlice } from './slice.ts'

/** Client-v0 schema definition names that slices carry. */
export type WireDefinition =
  | 'Agent'
  | 'Runtime'
  | 'Machine'
  | 'Mission'
  | 'Work'
  | 'Attention'
  | 'Message'
  | 'Resource'
  | 'TimelineEntry'
  | 'TerminalScreen'
  | 'Capabilities'
  | 'ErrorEnvelope'
  | 'SyncPeer'

export interface WireValue {
  /** JSON pointer from the slice file root (`/state/...` or `/timeline/...`). */
  readonly pointer: string
  readonly definition: WireDefinition
  readonly value: unknown
}

/**
 * Every client-v0 value inside a slice with its schema definition. The emitter derives `times`
 * from these; the decode gate decodes each one. Kit-only fields (offsets, casts, expectations) are
 * not wire values.
 */
export const wireValues = (slice: AnySlice): WireValue[] => {
  const out: WireValue[] = []
  const list = (base: string, values: readonly unknown[], definition: WireDefinition) =>
    values.forEach((value, index) => out.push({ pointer: `${base}/${index}`, definition, value }))
  switch (slice.kind) {
    case 'roster':
      list('/state/agents', slice.state.agents, 'Agent')
      list('/state/runtimes', slice.state.runtimes, 'Runtime')
      list('/state/machines', slice.state.machines, 'Machine')
      break
    case 'details':
      list('/state/missions', slice.state.missions, 'Mission')
      list('/state/work', slice.state.work, 'Work')
      break
    case 'attention':
      list('/state/attention', slice.state.attention, 'Attention')
      list('/state/messages', slice.state.messages, 'Message')
      break
    case 'conversation':
      slice.state.threads.forEach((thread, index) => list(`/state/threads/${index}/items`, thread.items, 'TimelineEntry'))
      break
    case 'terminal':
      slice.state.terminals.forEach((record, index) => {
        out.push({ pointer: `/state/terminals/${index}/runtime`, definition: 'Runtime', value: record.runtime })
        record.screens.forEach((screen, at) =>
          out.push({ pointer: `/state/terminals/${index}/screens/${at}/screen`, definition: 'TerminalScreen', value: screen.screen }),
        )
      })
      break
    case 'sync':
      out.push({ pointer: '/state/capabilities', definition: 'Capabilities', value: slice.state.capabilities })
      break
  }
  slice.timeline.forEach((event, index) => {
    const base = `/timeline/${index}`
    switch (event._tag) {
      case 'changes':
        list(`${base}/upserts`, event.upserts, 'Resource')
        break
      case 'entries':
      case 'replace':
        list(`${base}/items`, event.items, 'TimelineEntry')
        break
      case 'screen':
        out.push({ pointer: `${base}/screen`, definition: 'TerminalScreen', value: event.screen })
        break
      case 'http-error':
        out.push({ pointer: `${base}/envelope`, definition: 'ErrorEnvelope', value: event.envelope })
        break
      case 'notice':
        list(`${base}/peers`, event.peers, 'SyncPeer')
        break
      default:
        break
    }
  })
  return out
}
