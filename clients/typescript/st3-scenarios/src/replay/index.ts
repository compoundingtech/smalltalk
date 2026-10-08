import type {
  ActionRequest, ActionResult, CollectionName, CollectionSocket, CollectionSocketFactory,
  ErrorCode, ErrorEnvelope, Operation, Snapshot, SyncNotice,
  TerminalAttachment, TimelinePage,
} from '@smalltalk/st3-client'

import type { Clock } from '../clock.ts'
import { foldSlice } from '../kit/fold.ts'
import { SLICE_KINDS, type HttpCondition, type Selector, type SliceKind, type TimelineEvent, type WireResource } from '../kit/slice.ts'
import type { World } from '../kit/world.ts'

export { manualClock, realClock, type Clock, type ManualClock } from '../clock.ts'

export interface Replay {
  readonly socket: CollectionSocketFactory
  readonly fetch: typeof fetch
  readonly actions: ActionRequest[]
  readonly served: () => ReadonlySet<SliceKind>
  readonly close: () => void
}

type Filters = { status?: string; actor?: string; person?: string; owner?: string; state?: string }
type Subscription = {
  id: string
  collection: CollectionName | 'conversation' | 'terminal'
  limit: number
  filters: Filters
  conversation?: string
  terminal?: string
  incarnation?: string
  capability?: string
  ready: boolean
  window?: { items: WireResource[]; has_more: boolean }
}
type Connection = { socket: CollectionSocket; open: boolean; closed: boolean; subscriptions: Map<string, Subscription>; legacy: boolean }
const collectionKind = (collection: string): SliceKind | undefined => {
  switch (collection) {
    case 'agents': case 'runtimes': case 'machines': return 'roster'
    case 'missions': case 'work': return 'details'
    case 'attention': case 'messages': return 'attention'
    case 'conversation': case 'timeline': case 'sessions': return 'conversation'
    case 'terminal': case 'terminals': return 'terminal'
    case 'capabilities': return 'sync'
    default: return undefined
  }
}
const equal = (left: unknown, right: unknown): boolean => JSON.stringify(left) === JSON.stringify(right)
const matches = (sub: Subscription, selector: Selector): boolean =>
  sub.collection === selector.collection &&
  (!('conversation' in selector) || sub.conversation === selector.conversation) &&
  (!('terminal' in selector) || sub.terminal === selector.terminal)
const selectorKey = (selector: Selector): string => JSON.stringify(selector)
const filtered = (items: WireResource[], filters: Filters): WireResource[] => items.filter((item) => {
  if (filters.status !== undefined && (item.kind !== 'agent' || !('state' in item) || item.state !== filters.status)) return false
  if (filters.actor !== undefined && (item.kind !== 'work' || (!('assigned_to' in item) || item.assigned_to !== filters.actor) && (!('claimant' in item) || item.claimant !== filters.actor))) return false
  if (filters.person !== undefined && (item.kind !== 'attention' || !('person_id' in item) || item.person_id !== filters.person)) return false
  if (filters.owner !== undefined && (item.kind !== 'runtime' || !('owner_id' in item) || item.owner_id !== filters.owner)) return false
  if (filters.state !== undefined && (!('state' in item) || item.state !== filters.state)) return false
  return true
})
const record = (value: unknown): value is Record<string, unknown> => typeof value === 'object' && value !== null
const stringField = (value: Record<string, unknown>, key: string): string | undefined => typeof value[key] === 'string' ? value[key] : undefined
/** A conversation subscription names its thread by agent id, session id, or session id without `session/`. */
const followsThread = (sub: Subscription, thread: { agent: string; session_id: string }): boolean =>
  sub.conversation === thread.agent || sub.conversation === thread.session_id || `session/${sub.conversation}` === thread.session_id

const conditionKey = (route: string, when?: HttpCondition): string => JSON.stringify([route, when?.cursor ?? null, Object.entries(when?.query ?? {}).sort(([a], [b]) => a.localeCompare(b))])
const requestMatches = (url: URL, when?: HttpCondition): boolean =>
  (when?.cursor === undefined || url.searchParams.has('cursor') === (when.cursor === 'present')) &&
  Object.entries(when?.query ?? {}).every(([key, value]) => url.searchParams.get(key) === value)

/** Socket opens are clock tasks, not microtasks: manual-clock callers release them with advance(0). */
export const createReplay = (world: World, { clock }: { readonly clock: Clock }): Replay => {
  const consumed = new Set<SliceKind>()
  const actions: ActionRequest[] = []
  const connections = new Set<Connection>()
  const cancellations = new Set<() => void>()
  const held = new Set<string>()
  const leases = new Map<string, { terminal: string; incarnation: string; expires: number }>()
  const operations = new Map<string, Operation>()
  const overrides = new Map<string, Extract<TimelineEvent, { _tag: 'http-error' | 'http-raw' }>>()
  const repeatedErrors = new Map<string, Extract<TimelineEvent, { _tag: 'error' }>>()
  let sync: SyncNotice | undefined
  let closed = false
  let failedOpens = 0
  let blockedUntil = -Infinity
  let request = 0
  let capability = 0
  const offset = () => clock.now() - world.now
  const state = <K extends SliceKind>(kind: K) => foldSlice(world.slices[kind], offset()).state
  const events = SLICE_KINDS.flatMap((kind) => world.slices[kind].timeline.map((event) => ({ kind, event })))
    .sort((a, b) => a.event.at_ms - b.event.at_ms)
  const fence = (): Snapshot => {
    const store = events.reduce((store, { event }) => event.at_ms <= offset() ? Math.max(store, event.store) : store, 1)
    const changedAt = events.reduce((at, { event }) => event.at_ms <= offset() && event.store === store ? Math.min(at, event.at_ms) : at, Infinity)
    const host = world.cast.hosts[0]!.id
    return { id: `snapshot/${host}/${store}/scenario`, host_id: host, store_index: store,
      projection_version: 'client-projection.v0', created_at: new Date(world.now + (store === 1 || changedAt === Infinity ? 0 : changedAt)).toISOString() }
  }
  const schedule = (at: number, task: () => void) => {
    let cancel: () => void = () => undefined
    cancel = clock.schedule(at, () => { cancellations.delete(cancel); if (!closed) task() })
    cancellations.add(cancel)
  }
  const envelope = (value: unknown) => ({ api_version: 'st3.client.v0', request_id: `request/scenario-${++request}`, snapshot: fence(), value })
  const errorEnvelope = (code: ErrorCode, message: string, retryable = false): ErrorEnvelope => ({
    api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', request_id: `request/scenario-${++request}`,
    code, message, retryable, details: {},
  })
  const rows = (collection: string): WireResource[] => {
    switch (collection) {
      case 'agents': {
        const roster = state('roster')
        const order = new Map(roster.order.map((id, index) => [id, index]))
        return [...[...roster.agents].sort((a, b) => (order.get(a.id) ?? Infinity) - (order.get(b.id) ?? Infinity)), ...(roster.resources ?? [])]
      }
      case 'runtimes': return [...state('roster').runtimes, ...state('terminal').terminals.map((terminal) => terminal.runtime)]
      case 'terminals': return state('terminal').terminals.map((terminal) => terminal.runtime)
      case 'machines': return state('roster').machines
      case 'missions': return [...state('details').missions, ...(state('details').resources ?? [])]
      case 'work': return state('details').work
      case 'attention': return [...state('attention').attention, ...(state('attention').resources ?? [])]
      case 'messages': return state('attention').messages
      case 'operations': return [...operations.values()]
      default: return []
    }
  }
  const allRows = () => ['agents', 'runtimes', 'machines', 'missions', 'work', 'attention', 'messages', 'operations'].flatMap(rows)
  const windowOf = (sub: Subscription) => {
    const all = filtered(rows(sub.collection), sub.filters)
    return { items: all.slice(0, sub.limit), has_more: all.length > sub.limit }
  }
  const emit = (connection: Connection, frame: Record<string, unknown>) => {
    if (!connection.open || connection.closed) return
    if (!connection.legacy) connection.socket.onmessage?.({ data: JSON.stringify(frame) })
    else if (frame.kind === 'screen') connection.socket.onmessage?.({ data: JSON.stringify(envelope(frame.value)) })
    else if (frame.kind === 'error') connection.socket.onmessage?.({ data: JSON.stringify(errorEnvelope(typeof frame.code === 'string' ? frame.code : 'terminal-unavailable', String(frame.message), frame.retryable === true)) })
  }
  const refuse = (connection: Connection, sub: Subscription, code: string, retryable: boolean) => {
    emit(connection, { kind: 'error', id: sub.id, collection: sub.collection, code, message: code, retryable })
    connection.subscriptions.delete(sub.id)
  }
  const first = (connection: Connection, sub: Subscription) => {
    if (sub.ready) return
    for (const event of repeatedErrors.values()) {
      if (event.selector !== undefined && !matches(sub, event.selector)) continue
      emit(connection, { kind: 'error', ...(event.selector === undefined ? {} : { id: sub.id, collection: sub.collection }),
        message: event.message, retryable: event.retryable, ...(event.code === undefined ? {} : { code: event.code }) })
      connection.subscriptions.delete(sub.id)
      return
    }
    const kind = collectionKind(sub.collection)
    if (kind !== undefined && world.slices[kind].loading) return
    if ([...held].some((key) => matches(sub, JSON.parse(key) as Selector))) return
    sub.ready = true
    if (kind !== undefined) consumed.add(kind)
    switch (sub.collection) {
      case 'conversation': {
        const thread = state('conversation').threads.find((thread) => followsThread(sub, thread))
        if (thread === undefined) return refuse(connection, sub, 'not-found', false)
        emit(connection, { kind: 'conversation', id: sub.id, collection: 'conversation', session_id: thread.session_id,
          replace: true, items: thread.items.slice(-thread.page_size), has_more: thread.has_more || thread.items.length > thread.page_size })
        return
      }
      case 'terminal': {
        const terminal = state('terminal').terminals.find((item) => item.terminal === sub.terminal)
        if (terminal === undefined) return refuse(connection, sub, 'not-found', false)
        if (terminal.runtime.state === 'exited') return refuse(connection, sub, 'terminal-ended', false)
        if (sub.incarnation !== terminal.incarnation) return refuse(connection, sub, 'stale-fence', false)
        const lease = leases.get(sub.capability ?? '')
        if (lease === undefined || lease.terminal !== terminal.terminal || lease.incarnation !== terminal.incarnation || lease.expires <= clock.now()) return refuse(connection, sub, 'terminal-unavailable', true)
        const screen = terminal.screens.filter((item) => item.at_ms <= offset()).at(-1)?.screen
        if (screen !== undefined) emit(connection, { kind: 'screen', id: sub.id, collection: 'terminal', snapshot: fence(), value: screen })
        return
      }
      default: {
        const window = windowOf(sub)
        sub.window = window
        emit(connection, { kind: 'snapshot', id: sub.id, collection: sub.collection, snapshot: fence(), ...window, order: window.items.map((item) => item.id) })
      }
    }
  }
  const finishConnection = (connection: Connection, code: number, reason: string) => {
    if (connection.closed) return
    connection.closed = true
    connection.open = false
    connection.subscriptions.clear()
    connections.delete(connection)
    connection.socket.onclose?.({ code, reason })
  }
  const dispatch = (kind: SliceKind, event: TimelineEvent) => {
    switch (event._tag) {
      case 'thread-create': case 'terminal-create':
        for (const connection of connections) for (const sub of connection.subscriptions.values()) first(connection, sub)
        return
      case 'thread-remove': {
        const previous = foldSlice(world.slices.conversation, event.at_ms - 1).state.threads.find((thread) => thread.agent === event.agent)
        for (const connection of connections) for (const sub of connection.subscriptions.values()) {
          if (sub.collection === 'conversation' && (sub.conversation === event.agent || previous !== undefined && followsThread(sub, previous))) refuse(connection, sub, 'not-found', false)
        }
        return
      }
      case 'changes':
        for (const connection of connections) for (const sub of connection.subscriptions.values()) {
          if (!sub.ready || collectionKind(sub.collection) !== kind || sub.window === undefined) continue
          const previous = sub.window
          const next = windowOf(sub)
          if (equal(previous, next)) continue
          sub.window = next
          emit(connection, { kind: 'changes', id: sub.id, collection: sub.collection as CollectionName, snapshot: fence(),
            upserts: next.items.filter((item) => !equal(previous.items.find((old) => old.id === item.id), item)),
            removes: previous.items.filter((item) => !next.items.some((row) => row.id === item.id)).map((item) => item.id),
            order: next.items.map((item) => item.id), has_more: next.has_more })
        }
        return
      case 'entries': case 'replace': {
        const thread = state('conversation').threads.find((thread) => thread.agent === event.agent)
        if (thread === undefined) return
        for (const connection of connections) for (const sub of connection.subscriptions.values()) {
          if (!sub.ready || sub.collection !== 'conversation' || !followsThread(sub, thread)) continue
          emit(connection, { kind: 'conversation', id: sub.id, collection: 'conversation', session_id: thread.session_id,
            replace: event._tag === 'replace', items: event._tag === 'replace' ? thread.items.slice(-thread.page_size) : event.items,
            ...(event._tag === 'replace' ? { has_more: thread.has_more || thread.items.length > thread.page_size } : {}) })
        }
        return
      }
      case 'screen': case 'unavailable': case 'end': case 'incarnation': case 'terminal-remove':
        for (const connection of connections) for (const sub of connection.subscriptions.values()) {
          if (!sub.ready || sub.collection !== 'terminal' || sub.terminal !== event.terminal) continue
          if (event._tag === 'screen') {
            const terminal = state('terminal').terminals.find((terminal) => terminal.terminal === event.terminal)
            if (sub.incarnation === terminal?.incarnation) emit(connection, { kind: 'screen', id: sub.id, collection: 'terminal', snapshot: fence(), value: event.screen })
          } else refuse(connection, sub, event._tag === 'terminal-remove' ? 'not-found' : event._tag === 'unavailable' ? 'terminal-unavailable' : event._tag === 'end' ? 'terminal-ended' : 'stale-fence', event._tag === 'unavailable')
        }
        return
      case 'open-fail':
        if (event.opens !== undefined && event.opens !== 'all' && (!Number.isSafeInteger(event.opens) || event.opens < 1)) throw new Error('open-fail opens must be positive')
        failedOpens = event.opens === 'all' ? Infinity : event.opens ?? 1
        return
      case 'open-ok': failedOpens = 0; return
      case 'close':
        blockedUntil = Infinity
        for (const connection of [...connections]) finishConnection(connection, event.code, event.reason)
        return
      case 'reopen': blockedUntil = clock.now() + event.after_ms; return
      case 'http-error': case 'http-raw': {
        const key = conditionKey(event.route, event.when)
        overrides.delete(key)
        overrides.set(key, event)
        return
      }
      case 'http-ok':
        for (const [key, override] of overrides) {
          if (override.route === event.route && (event.when === undefined || key === conditionKey(event.route, event.when))) overrides.delete(key)
        }
        return
      case 'hold': held.add(selectorKey(event.selector)); return
      case 'release':
        held.delete(selectorKey(event.selector))
        for (const connection of connections) for (const sub of connection.subscriptions.values()) if (matches(sub, event.selector)) first(connection, sub)
        return
      case 'resync': case 'error':
        if (event._tag === 'error' && event.repeat) repeatedErrors.set(event.selector === undefined ? '*' : selectorKey(event.selector), event)
        for (const connection of connections) {
          if (event._tag === 'error' && event.selector === undefined) {
            emit(connection, { kind: 'error', message: event.message, retryable: event.retryable, ...(event.code === undefined ? {} : { code: event.code }) })
            continue
          }
          const selector = event.selector
          if (selector === undefined) continue
          for (const sub of connection.subscriptions.values()) if (matches(sub, selector)) {
            if (event._tag === 'error') {
              emit(connection, { kind: 'error', id: sub.id, collection: sub.collection,
                retryable: event.retryable, message: event.message, ...(event.code === undefined ? {} : { code: event.code }) })
              connection.subscriptions.delete(sub.id)
            } else {
              emit(connection, { kind: 'resync', id: sub.id, collection: sub.collection, retryable: true,
                ...(event.code === undefined ? {} : { code: event.code }),
                ...(event.message === undefined ? {} : { message: event.message }) })
            }
          }
        }
        return
      case 'error-clear': repeatedErrors.delete(event.selector === undefined ? '*' : selectorKey(event.selector)); return
      case 'notice': sync = { state: event.peers.some((peer) => peer.diverged_since != null) ? 'diverged' : 'catching-up', peers: event.peers }; return
      case 'notice-clear': sync = undefined; return
      default: {
        const unreachable: never = event
        throw new Error(`Unknown replay event: ${JSON.stringify(unreachable)}`)
      }
    }
  }
  for (const { kind, event } of events) {
    if (event.at_ms <= offset()) { if (kind === 'sync') dispatch(kind, event) }
    else schedule(world.now + event.at_ms, () => dispatch(kind, event))
  }
  const socket: CollectionSocketFactory = (address, protocols) => {
    const url = new URL(address)
    const connection: Connection = { socket: {
      onopen: null, onmessage: null, onerror: null, onclose: null,
      close: (code = 1000, reason = '') => finishConnection(connection, code, reason),
      send: (data) => {
        if (!connection.open || connection.closed) return
        const command: unknown = JSON.parse(data)
        if (!record(command) || typeof command.id !== 'string') return
        if (command.kind === 'unsubscribe') { connection.subscriptions.delete(command.id); return }
        if (command.kind !== 'subscribe') return
        const collection = stringField(command, 'collection')
        if (collection !== 'agents' && collection !== 'missions' && collection !== 'work' && collection !== 'attention' && collection !== 'conversation' && collection !== 'terminal' && collection !== 'glasses' && collection !== 'arrangements') return
        const sub: Subscription = { id: command.id, collection, ready: false,
          limit: typeof command.limit === 'number' ? Math.max(1, Math.min(200, Math.trunc(command.limit))) : 100,
          filters: { status: stringField(command, 'status'), actor: stringField(command, 'actor'), person: stringField(command, 'person') },
          conversation: stringField(command, 'conversation'), terminal: stringField(command, 'terminal'),
          incarnation: stringField(command, 'incarnation'), capability: stringField(command, 'capability') }
        connection.subscriptions.set(sub.id, sub)
        first(connection, sub)
      },
    }, open: false, closed: false, subscriptions: new Map(), legacy: protocols.includes('st3.client.terminal.v0') }
    connections.add(connection)
    schedule(clock.now(), () => {
      if (connection.closed) return
      if (failedOpens > 0 || clock.now() < blockedUntil) {
        if (failedOpens > 0) failedOpens -= 1
        connection.socket.onerror?.({})
        finishConnection(connection, 1006, '')
        return
      }
      connection.open = true
      connection.socket.onopen?.()
      if (connection.legacy) {
        const terminal = decodeURIComponent(url.pathname.split('/').at(-2) ?? '')
        const sub: Subscription = { id: 'terminal', collection: 'terminal', terminal, incarnation: url.searchParams.get('incarnation') ?? undefined,
          capability: protocols.find((protocol) => protocol.startsWith('st3.cap.'))?.slice(8), ready: false, filters: {}, limit: 1 }
        connection.subscriptions.set(sub.id, sub)
        first(connection, sub)
      }
    })
    return connection.socket
  }
  const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } })
  const fetchImpl: typeof fetch = async (input, init) => {
    if (closed) throw new DOMException('Replay closed', 'AbortError')
    const req = new Request(input, init)
    const url = new URL(req.url)
    const parts = url.pathname.replace(/^\/v1\/client\/?/, '').split('/').map(decodeURIComponent)
    const route = parts[0] ?? ''
    const family = parts.includes('timeline') ? 'timeline' : route
    const routes = [url.pathname, family, ...(route !== 'capabilities' && route !== 'actions' && route !== 'events' && family !== 'timeline' ? ['resources'] : [])]
    const override = routes.flatMap((route) => [...overrides.values()].reverse().filter((event) => event.route === route && requestMatches(url, event.when)))[0]
    if (override?._tag === 'http-error') return json(override.envelope, override.status)
    if (override?._tag === 'http-raw') return new Response(override.body, { status: override.status, headers: { 'Content-Type': override.content_type } })
    const kind = collectionKind(family)
    const sources: SliceKind[] = route === 'resources' ? ['roster', 'details', 'attention', 'terminal']
      : route === 'runtimes' ? ['roster', 'terminal'] : kind === undefined ? [] : [kind]
    if (sources.some((kind) => world.slices[kind].loading)) return new Promise<Response>((_resolve, reject) => {
      const abort = () => { cancellations.delete(abort); reject(new DOMException('Replay read cancelled', 'AbortError')) }
      cancellations.add(abort)
      req.signal.addEventListener('abort', abort, { once: true })
      if (req.signal.aborted) abort()
    })
    for (const kind of sources) consumed.add(kind)
    if (route === 'capabilities' && req.method === 'GET') return json(envelope(state('sync').capabilities))
    if (route === 'actions' && req.method === 'POST') {
      const action = await req.json() as ActionRequest
      const snapshot = fence()
      const operationId = `operation/scenario-${operations.size + 1}`
      const result: ActionResult = { kind: 'action-result', action_id: action.id, affected_ids: [], operation_id: operationId, snapshot_id: snapshot.id, status: 'accepted' }
      if (action.type === 'terminal.attach') {
        consumed.add('terminal')
        const terminal = state('terminal').terminals.find((terminal) => terminal.terminal === action.parameters.target_id || terminal.runtime.id === action.parameters.target_id)
        if (terminal === undefined) return json(errorEnvelope('not-found', 'Terminal not found'), 404)
        if (terminal.runtime.state === 'exited') return json(errorEnvelope('terminal-ended', 'Terminal ended'), 409)
        if (action.fence.runtime_incarnation !== terminal.incarnation) return json(errorEnvelope('stale-fence', 'Terminal incarnation changed'), 409)
        const streamCapability = `scenario-terminal-stream-capability-${++capability}`
        const expires = clock.now() + 300_000
        leases.set(streamCapability, { terminal: terminal.terminal, incarnation: terminal.incarnation, expires })
        const attachment: TerminalAttachment = { attachment_id: `attachment/scenario-${capability}`, terminal_id: terminal.terminal,
          owner_host_id: terminal.runtime.owner_host_id, runtime_incarnation: terminal.incarnation, stream_capability: streamCapability,
          stream_url: `/v1/client/terminals/${encodeURIComponent(terminal.terminal)}/stream`, state: 'available', reusable: true,
          ttl_s: 300, expires_at: new Date(expires).toISOString() }
        result.terminal_attachment = attachment
        result.status = 'completed'
      } else actions.push(action)
      operations.set(operationId, { kind: 'operation', id: operationId, revision: '1', updated_at: new Date(clock.now()).toISOString(),
        component: 'action', severity: 'info', state: 'completed', summary: 'Scenario action accepted', targets: result.affected_ids })
      return json(envelope(result), result.status === 'accepted' ? 202 : 200)
    }
    if (req.method !== 'GET') return json(errorEnvelope('unsupported-capability', 'Route not supported by this world'), 405)
    const max = state('sync').capabilities.limits.max_page_items
    const limit = Number(url.searchParams.get('limit') ?? max)
    if (!Number.isSafeInteger(limit) || limit < 1 || limit > max) return json(errorEnvelope('validation-failed', 'Invalid page limit'), 400)
    const cursor = url.searchParams.get('cursor')
    const start = cursor === null ? 0 : Number(cursor.replace(/^scenario-cursor\//, ''))
    if (!Number.isSafeInteger(start) || start < 0) return json(errorEnvelope('page-cursor-expired', 'Invalid page cursor'), 410)
    const pageInfo = (length: number, next: number) => ({ limit, has_more: next < length, ...(next < length ? { next_cursor: `scenario-cursor/${next}` } : {}) })
    if (family === 'timeline') {
      const session = parts[1]
      const thread = state('conversation').threads.find((thread) => thread.session_id === session || thread.session_id.replace(/^session\//, '') === session)
      if (thread === undefined) return json(errorEnvelope('not-found', 'Session not found'), 404)
      const end = Math.max(0, thread.items.length - start)
      const items = thread.items.slice(Math.max(0, end - limit), end)
      const value: TimelinePage = { kind: 'timeline-page', session_id: thread.session_id, items, page: pageInfo(thread.items.length, start + items.length) }
      return json(envelope(value))
    }
    if (route === 'events') return json(envelope({ kind: 'event-page', items: [], has_more: false,
      oldest_cursor: state('sync').capabilities.oldest_event_cursor, resume_cursor: state('sync').capabilities.event_cursor }))
    if (parts.length > 1) {
      if (route === 'terminals' && parts[2] === 'screen') {
        const terminal = state('terminal').terminals.find((terminal) => terminal.terminal === parts[1])
        const screen = terminal?.screens.filter((item) => item.at_ms <= offset()).at(-1)?.screen
        return screen === undefined ? json(errorEnvelope('not-found', 'Screen not found'), 404) : json(envelope(screen))
      }
      const value = rows(route).find((item) => item.id === parts[1] || (route === 'terminals' && item.kind === 'runtime' && 'terminal_id' in item && item.terminal_id === parts[1]))
      return value === undefined ? json(errorEnvelope('not-found', 'Resource not found'), 404) : json(envelope(value))
    }
    const filters = Object.fromEntries([...url.searchParams].filter(([key]) => key !== 'cursor' && key !== 'limit'))
    if (route === 'resources') {
      for (const kind of ['roster', 'details', 'attention', 'terminal'] as const) consumed.add(kind)
      const resources = allRows().filter((item) => (filters.kind === undefined || item.kind === filters.kind) && (filters.subject_prefix === undefined || item.id.startsWith(filters.subject_prefix)))
      const items = resources.map((item) => ({ id: item.id, kind: item.kind, facts: item,
        observed_at: item.updated_at, opened_by: 'owner_id' in item ? item.owner_id : null, opened_by_run: 'owner_run_id' in item ? item.owner_run_id ?? null : null }))
        .filter((item) => filters.opened_by === undefined || item.opened_by === filters.opened_by)
      const value = { kind: 'page', collection: 'resources', filters, items: items.slice(start, start + limit), page: pageInfo(items.length, start + limit), ...(sync === undefined ? {} : { sync }) }
      return json(envelope(value))
    }
    const items = filtered(rows(route), filters)
    const value = { kind: 'page', collection: route, filters, items: items.slice(start, start + limit), page: pageInfo(items.length, start + limit), ...(sync === undefined ? {} : { sync }) }
    return json(envelope(value))
  }
  return { socket, fetch: fetchImpl, actions, served: () => new Set(consumed), close: () => {
    if (closed) return
    closed = true
    for (const cancel of cancellations) cancel()
    cancellations.clear()
    for (const connection of [...connections]) finishConnection(connection, 1000, '')
  } }
}
