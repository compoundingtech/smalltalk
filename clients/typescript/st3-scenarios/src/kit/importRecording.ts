import clientSchema from '../../../../../docs/st3/client-v0/schemas/client-v0.schema.json' with { type: 'json' }
import type { Agent, Machine, Resource, Runtime, Snapshot, TerminalAttachment, TerminalScreen, TimelineEntry } from '@smalltalk/st3-client'
import type { RecordedFrame, RecordedHttp, RecordedTransport } from './recording.ts'
import type { ChangesEvent, ConversationEvent, ConversationThread, TerminalEvent, TerminalRecord } from './slice.ts'
import type { Slices } from './world.ts'
import { ANCHOR, ANCHOR_MS, escapePointerToken, formatTimestamp, nowMs, parseTimestamp, rebase, type TimePointer } from './time.ts'
import { SLICE_KINDS } from './slice.ts'
import { wireValues } from './wire.ts'

export type JsonValue = null | boolean | number | string | JsonValue[] | JsonObject
export interface JsonObject { [key: string]: JsonValue }
export interface RecordingRow {
  readonly ordinal: number
  readonly socket_id: string | null
  readonly receive_ms: number
  readonly kind: 'header' | 'http' | 'attach' | 'command' | 'frame' | 'baseline' | 'harness' | 'pty_output'
  readonly value: JsonObject
}
export interface RecordingTurn {
  readonly prompt: string
  readonly answer: string
  readonly timestamp: string
  readonly command?: string
  readonly agent?: string
  readonly tool?: { readonly id: string; readonly name: string; readonly input: JsonValue; readonly result: JsonValue }
  readonly revisions?: readonly string[]
}
export interface RecordingScript {
  readonly format_version: 1 | 2
  readonly world: string
  readonly agent: string
  readonly host: string
  readonly person: string
  readonly workspace: string
  readonly id_map: Readonly<Record<string, string>>
  readonly agents?: readonly { readonly agent: string; readonly host: string }[]
  readonly historical_turns?: number
  readonly empty_baseline?: boolean
  readonly anchor?: string
  readonly walk_pages?: boolean
  readonly reconnect_after?: readonly number[]
  readonly turns: readonly RecordingTurn[]
  readonly reads: readonly string[]
}
const fail: (pointer: string, message: string) => never = (pointer, message) => { throw new Error(`${pointer}: ${message}`) }
const json = (value: unknown, pointer: string): JsonValue => {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return value
  if (typeof value === 'number' && Number.isFinite(value)) return value
  if (Array.isArray(value)) return value.map((item: unknown, index) => json(item, `${pointer}/${index}`))
  if (typeof value === 'object' && value !== null) return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, json(item, `${pointer}/${key}`)]))
  return fail(pointer, 'expected JSON value')
}
const object = (value: JsonValue | undefined, pointer = ''): JsonObject => value !== undefined && value !== null && typeof value === 'object' && !Array.isArray(value) ? value : fail(pointer, 'expected object')
const array = (value: JsonValue | undefined, pointer = ''): JsonValue[] => Array.isArray(value) ? value : fail(pointer, 'expected array')
const text = (value: JsonValue | undefined, pointer = ''): string => typeof value === 'string' ? value : fail(pointer, 'expected string')
const number = (value: JsonValue | undefined, pointer = ''): number => typeof value === 'number' && Number.isFinite(value) ? value : fail(pointer, 'expected number')
const integer = (value: JsonValue | undefined, pointer = ''): number => Number.isInteger(number(value, pointer)) ? number(value, pointer) : fail(pointer, 'expected integer')
const bool = (value: JsonValue | undefined, pointer = ''): boolean => typeof value === 'boolean' ? value : fail(pointer, 'expected boolean')
const definitions = object(object(json(clientSchema, '')).$defs)
const node = (name: string): JsonObject => object(definitions[name], `/$defs/${name}`)
const resolve = (schema: JsonObject): JsonObject => typeof schema.$ref === 'string' ? node(schema.$ref.split('/').at(-1) ?? '') : schema
const matches = (raw: JsonObject, value: JsonValue): boolean => {
  const s = resolve(raw)
  if (s.const !== undefined && s.const !== value) return false
  if (Array.isArray(s.enum) && !s.enum.includes(value)) return false
  const types = s.type === undefined ? [] : Array.isArray(s.type) ? s.type : [s.type]
  if (types.length && !types.some((type) => type === 'null' ? value === null : type === 'array' ? Array.isArray(value) : type === 'object' ? value !== null && typeof value === 'object' && !Array.isArray(value) : type === 'integer' ? typeof value === 'number' && Number.isInteger(value) || typeof value === 'string' && /^store#[1-9]\d*$/u.test(value) : typeof value === type)) return false
  if (typeof value === 'number') {
    if (typeof s.minimum === 'number' && value < s.minimum || typeof s.maximum === 'number' && value > s.maximum) return false
  }
  if (typeof value === 'string' && !/^(?:store|cursor|request|operation|attachment|capability|incarnation)#[1-9]\d*$/u.test(value)) {
    if (typeof s.minLength === 'number' && value.length < s.minLength || typeof s.maxLength === 'number' && value.length > s.maxLength) return false
    if (typeof s.pattern === 'string' && !new RegExp(s.pattern, 'u').test(value)) return false
  }
  if (Array.isArray(value)) {
    if (typeof s.minItems === 'number' && value.length < s.minItems || typeof s.maxItems === 'number' && value.length > s.maxItems) return false
    if (s.uniqueItems === true && new Set(value.map((item) => JSON.stringify(item))).size !== value.length) return false
    if (s.items !== undefined && !value.every((item) => matches(object(s.items), item))) return false
  }
  if (value !== null && typeof value === 'object' && !Array.isArray(value)) {
    if (Array.isArray(s.required) && s.required.some((key) => typeof key === 'string' && value[key] === undefined)) return false
    const properties = s.properties === undefined ? {} : object(s.properties)
    if (Object.entries(properties).some(([key, property]) => value[key] !== undefined && !matches(object(property), value[key]!))) return false
    if (s.additionalProperties === false && Object.keys(value).some((key) => properties[key] === undefined)) return false
  }
  if (Array.isArray(s.allOf) && !s.allOf.every((branch) => matches(object(branch), value))) return false
  for (const key of ['oneOf', 'anyOf']) if (Array.isArray(s[key]) && !s[key].some((branch) => matches(object(branch), value))) return false
  if (s.if !== undefined && s.then !== undefined && matches(object(s.if), value) && !matches(object(s.then), value)) return false
  return true
}
/** Check the legacy kit boundary without a rich-schema round trip changing field presence. */
const legacyShape = (raw: JsonObject, value: JsonValue): boolean => {
  const schema = resolve(raw)
  for (const keyword of ['allOf', 'anyOf', 'oneOf']) {
    const alternatives = schema[keyword]
    if (!Array.isArray(alternatives)) continue
    const branches = alternatives.map((part) => object(part))
    if (keyword === 'allOf') {
      if (!branches.every((part) => legacyShape(part, value))) return false
    } else if (!branches.some((part) => matches(part, value) && legacyShape(part, value))) return false
  }
  if (Array.isArray(value) && schema.items !== undefined) return value.every((item) => legacyShape(object(schema.items), item))
  if (value === null || typeof value !== 'object' || Array.isArray(value) || schema.properties === undefined) return true
  const required = schema.required === undefined ? [] : array(schema.required)
  for (const [key, property] of Object.entries(object(schema.properties))) {
    const item = value[key]
    if (item === undefined) continue
    if (item === null && !required.includes(key)) return false
    if (!legacyShape(object(property), item)) return false
  }
  return true
}

const equalJson = (left: JsonValue, right: JsonValue): boolean => {
  if (left === right) return true
  if (Array.isArray(left)) return Array.isArray(right) && left.length === right.length && left.every((item, index) => right[index] !== undefined && equalJson(item, right[index]))
  if (left === null || right === null || typeof left !== 'object' || typeof right !== 'object' || Array.isArray(right)) return false
  const keys = Object.keys(left)
  return keys.length === Object.keys(right).length && keys.every((key) => right[key] !== undefined && equalJson(left[key]!, right[key]!))
}
const snapshotWire = (value: unknown): value is Snapshot => matches(node('Snapshot'), json(value, '')) && legacyShape(node('Snapshot'), json(value, ''))
const terminalScreenWire = (value: unknown): value is TerminalScreen => matches(node('TerminalScreen'), json(value, '')) && legacyShape(node('TerminalScreen'), json(value, ''))
const timelineEntryWire = (value: unknown): value is TimelineEntry => matches(node('TimelineEntry'), json(value, '')) && legacyShape(node('TimelineEntry'), json(value, ''))
const numericSnapshot = (value: JsonValue | undefined): Snapshot => {
  const s = object(value)
  const store = typeof s.store_index === 'string' ? Number(s.store_index.replace(/^store#/u, '')) : number(s.store_index)
  const wire: unknown = { ...s, store_index: store }
  if (!snapshotWire(wire)) fail('/snapshot', 'nullable optional field is not representable by the kit wire model')
  return wire
}
const screenValue = (value: JsonValue | undefined): TerminalScreen => {
  const wire: unknown = value
  if (!terminalScreenWire(wire)) fail('/screen', 'nullable optional field is not representable by the kit wire model')
  return wire
}
const entries = (value: JsonValue | undefined): TimelineEntry[] => array(value).map((item) => {
  const wire: unknown = item
  if (!timelineEntryWire(wire)) fail('/entry', 'nullable optional field is not representable by the kit wire model')
  return wire
})
const resourceWire = (value: unknown): value is Resource => matches(node('Resource'), json(value, '')) && legacyShape(node('Resource'), json(value, ''))

export const importRecording = (script: Pick<RecordingScript, 'format_version' | 'world' | 'agent' | 'host' | 'id_map'>, rows: readonly RecordingRow[]): { slices: Slices; recording: RecordedTransport } => {
  const source = { _tag: 'recorded', recording: script.format_version === 1 ? `${script.world}-proof` : script.world } as const
  const common = { variant: 'default', source, decode: 'strict', loading: false } as const
  const frames: RecordedFrame[] = []
  const http: RecordedHttp[] = []
  const conversation: ConversationEvent[] = []
  const terminal: TerminalEvent[] = []
  const conversationHistory: Extract<ConversationEvent, { readonly _tag: 'replace' }>[] = []
  const rosterHistory: ChangesEvent[] = []
  const screenHistory: { at_ms: number; screen: TerminalScreen }[] = []
  const threads: ConversationThread[] = []
  const roster: ChangesEvent[] = []
  const rosterResources: Resource[] = []
  const knownThreads = new Set<string>()
  const knownTerminals = new Set<string>()
  const subscriptions = new Map<string, string>()
  const captureResources = (items: JsonValue | undefined, at_ms: number, version: number): Resource[] => {
    const resources = array(items).map((item) => {
      const wire: unknown = item
      if (!resourceWire(wire)) fail('/resource', 'nullable optional resource field is not representable')
      return wire
    })
    rosterHistory.push({ _tag: 'changes', at_ms, store: version, upserts: resources, removes: [] })
    if (at_ms <= 0) {
      for (const resource of resources) {
        const index = rosterResources.findIndex((item) => item.id === resource.id)
        if (index === -1) rosterResources.push(resource)
        else rosterResources[index] = resource
      }
    } else if (resources.length > 0) {
      roster.push({ _tag: 'changes', at_ms, store: version, upserts: resources, removes: [] })
    }
    return resources
  }
  const terminals: TerminalRecord[] = []
  const agent = script.id_map[script.agent] ?? script.agent
  const host = script.id_map[script.host] ?? script.host
  let store = 1
  let attachment: TerminalAttachment | undefined
  for (const row of rows) {
    const v = row.value
    if (row.kind === 'command' && v.kind === 'subscribe' && v.collection === 'conversation') {
      subscriptions.set(`${row.socket_id}/${text(v.id)}`, text(v.conversation))
    }
    if (row.kind === 'http' && object(v.body).value !== undefined) {
      const envelope = object(v.body)
      const value = object(envelope.value)
      const snapshot = numericSnapshot(envelope.snapshot)
      const metadata = { ordinal: row.ordinal, at_ms: row.receive_ms, method: text(v.method), path: text(v.path), query: text(v.query), status: integer(v.status), request_id: text(envelope.request_id), snapshot }
      if (value.kind === 'action-result') {
        const a = object(value.terminal_attachment)
        const state = text(a.state)
        if (state !== 'available' && state !== 'consumed' && state !== 'detached' && state !== 'expired') fail('/attachment/state', 'invalid state')
        attachment = { attachment_id: text(a.attachment_id), terminal_id: text(a.terminal_id), runtime_incarnation: text(a.runtime_incarnation), owner_host_id: text(a.owner_host_id), stream_url: text(a.stream_url), stream_capability: a.stream_capability === null ? null : text(a.stream_capability), state, expires_at: text(a.expires_at), ...(a.reusable === undefined ? {} : { reusable: bool(a.reusable) }), ...(a.ttl_s === undefined ? {} : { ttl_s: integer(a.ttl_s) }), ...(a.retry_hint === undefined ? {} : { retry_hint: a.retry_hint === null ? null : 'reattach' }) }
        http.push({ ...metadata, value: { kind: 'attach', action_id: text(value.action_id), operation_id: text(value.operation_id), snapshot_id: text(value.snapshot_id), attachment } })
      } else if (value.kind === 'terminal-screen') {
        const history = screenHistory.length
        screenHistory.push({ at_ms: row.receive_ms, screen: screenValue(value) })
        http.push({ ...metadata, value: { kind: 'screen', terminal: text(value.terminal_id), history } })
      } else {
        const p = object(value.page)
        const page = { limit: integer(p.limit), has_more: bool(p.has_more),
          ...(p.next_cursor === undefined ? {} : { next_cursor: text(p.next_cursor) }),
          ...(p.cursor_expires_at === undefined ? {} : { cursor_expires_at: text(p.cursor_expires_at) }) }
        if (value.kind === 'timeline-page') {
          const pageEntries = entries(value.items)
          const items = pageEntries.map((entry) => {
            const blocks = object(object(array(value.items).find((item) => object(item).id === entry.id)).body).blocks !== undefined
            const project = (candidate: TimelineEntry): unknown => {
              if (blocks || candidate.body === null || typeof candidate.body !== 'object') return candidate
              const { blocks: _blocks, ...body } = object(json(candidate.body, '/entry/body'))
              return { ...candidate, body }
            }
            const equal = (candidate: TimelineEntry): boolean => equalJson(json(project(candidate), '/entry'), json(entry, '/entry'))
            let event = -1
            for (let index = conversation.length - 1; index >= 0; index -= 1) {
              const candidate = conversation[index]
              if (candidate !== undefined && (candidate._tag === 'entries' || candidate._tag === 'replace') &&
                candidate.items.some((item) => item.id === entry.id && equal(item))) {
                event = index
                break
              }
            }
            if (event !== -1) return { id: entry.id, blocks, event }
            let history = -1
            for (let index = conversationHistory.length - 1; index >= 0; index -= 1) {
              if (conversationHistory[index]?.items.some((item) => item.id === entry.id && equal(item))) {
                history = index
                break
              }
            }
            if (history === -1) {
              history = conversationHistory.length
              conversationHistory.push({ _tag: 'replace', at_ms: row.receive_ms, store: snapshot.store_index,
                agent: decodeURIComponent(text(v.path).slice('/v1/client/sessions/'.length, -'/timeline'.length)),
                session_id: text(value.session_id), items: [entry], has_more: false })
            }
            return { id: entry.id, blocks, history }
          })
          const route = text(v.path)
          if (!route.startsWith('/v1/client/sessions/') || !route.endsWith('/timeline')) fail(`/ordinal/${row.ordinal}`, 'invalid timeline route')
          const routeAgent = decodeURIComponent(route.slice('/v1/client/sessions/'.length, -'/timeline'.length))
          http.push({ ...metadata, value: { kind: 'timeline', agent: routeAgent, session_id: text(value.session_id), items, page } })
        }
        else if (value.kind === 'page' && value.collection === 'agents') {
          const resources = captureResources(value.items, row.receive_ms, snapshot.store_index)
          const filters: Record<string, string> = {}
          for (const [key, item] of Object.entries(object(value.filters))) filters[key] = text(item)
          http.push({ ...metadata, value: { kind: 'agents', history: rosterHistory.length - 1, items: resources.map((item) => item.id), page, filters } })
        } else fail(`/ordinal/${row.ordinal}`, 'unsupported HTTP capture')
      }
    }
    if (row.kind !== 'frame') continue
    if (v.kind === 'conversation') {
      const agent = subscriptions.get(`${row.socket_id}/${text(v.id)}`)
      if (agent === undefined) fail(`/ordinal/${row.ordinal}`, 'conversation frame has no captured subscription')
      const items = entries(v.items)
      const replace = bool(v.replace)
      const selector = { collection: 'conversation', conversation: agent } as const
      const boundary = { ordinal: row.ordinal, at_ms: row.receive_ms, selector, socket_id: row.socket_id ?? undefined, subscription_id: text(v.id), kind: 'conversation', replace, ...(v.has_more === undefined ? {} : { has_more: bool(v.has_more) }) } as const
      if (row.receive_ms <= 0) {
        const previous = threads.find((thread) => thread.agent === agent)
        const accumulated = new Map(previous?.items.map((item) => [item.id, item]))
        for (const item of items) accumulated.set(item.id, item)
        const thread = { agent, session_id: text(v.session_id), items: [...accumulated.values()].sort((a, b) => a.sequence - b.sequence), page_size: 50, has_more: v.has_more === undefined ? false : bool(v.has_more) }
        const index = threads.findIndex((item) => item.agent === agent)
        if (index === -1) threads.push(thread)
        else threads[index] = thread
        knownThreads.add(agent)
        const history = conversationHistory.length
        conversationHistory.push({ _tag: 'replace', at_ms: row.receive_ms, store, agent, session_id: text(v.session_id), items, has_more: thread.has_more })
        frames.push({ ...boundary, history })
      } else {
        if (!knownThreads.has(agent)) {
          conversation.push({ _tag: 'thread-create', at_ms: row.receive_ms, store, thread: { agent, session_id: text(v.session_id), items: [], page_size: 50, has_more: false } })
          knownThreads.add(agent)
        }
        const event = conversation.length
        conversation.push(replace ? { _tag: 'replace', at_ms: row.receive_ms, store, agent, session_id: text(v.session_id), items, has_more: v.has_more === undefined ? false : bool(v.has_more) } : { _tag: 'entries', at_ms: row.receive_ms, store, agent, items })
        frames.push({ ...boundary, event })
      }
    } else if (v.kind === 'screen') {
      const snapshot = numericSnapshot(v.snapshot)
      store = snapshot.store_index
      const screen = screenValue(v.value)
      const selector = { collection: 'terminal', terminal: screen.terminal_id } as const
      const runtimeKey = `${screen.terminal_id.slice('terminal/'.length)}-shell`
      const runtime: Runtime = { id: `runtime/${runtimeKey}`, kind: 'runtime', revision: screen.revision, updated_at: snapshot.created_at, desired_revision: null, incarnation_id: screen.runtime_incarnation, owner_host_id: host, owner_id: agent, runtime_id: runtimeKey, runtime_kind: 'terminal', state: 'running', terminal_id: screen.terminal_id }
      const pastScreens = rows.filter((r) =>
        r.kind === 'frame' && r.receive_ms <= 0 && r.value.kind === 'screen' && object(r.value.value).terminal_id === screen.terminal_id ||
          r.kind === 'http' && r.receive_ms <= Math.max(0, row.receive_ms) && object(r.value.body).value !== undefined && object(object(r.value.body).value).kind === 'terminal-screen' && object(object(r.value.body).value).terminal_id === screen.terminal_id)
        .map((r) => ({ at_ms: r.receive_ms, screen: screenValue(r.kind === 'frame' ? r.value.value : object(r.value.body).value) }))
        .sort((a, b) => a.at_ms - b.at_ms)
      const attached = rows.find((r) => r.kind === 'http' && object(r.value.body).value !== undefined &&
        object(object(r.value.body).value).kind === 'action-result' &&
        object(object(object(r.value.body).value).terminal_attachment).terminal_id === screen.terminal_id)
      const streamUrl = attached === undefined ? undefined : text(object(object(object(attached.value.body).value).terminal_attachment).stream_url)
      const terminalOwner = streamUrl === undefined ? agent : decodeURIComponent(new URL(streamUrl, 'http://scenario.invalid').pathname.slice('/v1/client/terminals/'.length, -'/stream'.length))
      const record: TerminalRecord = { terminal: screen.terminal_id, owner: terminalOwner, runtime: { ...runtime, owner_id: terminalOwner }, incarnation: screen.runtime_incarnation, cast: { header: { version: 2, width: screen.columns, height: screen.rows, ...(screen.title === undefined ? {} : { title: screen.title }) }, started_at_ms: 0, events: rows.filter((r) => r.kind === 'pty_output' && r.value.terminal === screen.terminal_id).map((r): [number, 'o', string] => [r.receive_ms / 1000, 'o', text(r.value.text)]) }, screens: pastScreens }
      if (row.receive_ms <= 0) {
        const index = terminals.findIndex((item) => item.terminal === screen.terminal_id)
        if (index === -1) terminals.push(record)
        else terminals[index] = record
        knownTerminals.add(screen.terminal_id)
        const screen_index = pastScreens.findIndex((item) => item.at_ms === row.receive_ms && equalJson(json(item.screen, '/screen'), json(screen, '/screen')))
        frames.push({ ordinal: row.ordinal, at_ms: row.receive_ms, selector, socket_id: row.socket_id ?? undefined, subscription_id: text(v.id), kind: 'screen', snapshot, screen_index })
      } else {
        if (!knownTerminals.has(screen.terminal_id)) {
          const firstScreen = pastScreens[0]
          const witness = rows.find((r) => r.kind === 'http' && r.receive_ms === firstScreen?.at_ms && object(r.value.body).value !== undefined && object(object(r.value.body).value).terminal_id === screen.terminal_id)
          terminal.push({ _tag: 'terminal-create', at_ms: firstScreen?.at_ms ?? row.receive_ms,
            store: witness === undefined ? store : numericSnapshot(object(witness.value.body).snapshot).store_index, record })
          knownTerminals.add(screen.terminal_id)
        }
        const event = terminal.length
        terminal.push({ _tag: 'screen', terminal: screen.terminal_id, screen, at_ms: row.receive_ms, store })
        frames.push({ ordinal: row.ordinal, at_ms: row.receive_ms, selector, socket_id: row.socket_id ?? undefined, subscription_id: text(v.id), kind: 'screen', snapshot, event })
      }
    } else if (v.kind === 'snapshot') {
      const collection = text(v.collection)
      if (collection !== 'agents' && collection !== 'terminals') fail(`/ordinal/${row.ordinal}`, 'unsupported recorded collection')
      const snapshot = numericSnapshot(v.snapshot)
      store = snapshot.store_index
      const resources = captureResources(v.items, row.receive_ms, store)
      frames.push({ ordinal: row.ordinal, at_ms: row.receive_ms, selector: { collection }, socket_id: row.socket_id ?? undefined, subscription_id: text(v.id), kind: 'snapshot', snapshot,
        history: rosterHistory.length - 1, item_ids: resources.map((item) => item.id), order: array(v.order).map((id) => text(id)), has_more: bool(v.has_more) })
    } else fail(`/ordinal/${row.ordinal}`, 'unsupported frame capture')
  }
  if (knownTerminals.size === 0 || !attachment) fail('/terminal', 'missing captured screen or attachment')
  return { slices: {
    roster: { ...common, kind: 'roster', state: {
      agents: rosterResources.filter((item): item is Agent => item.kind === 'agent'),
      runtimes: rosterResources.filter((item): item is Runtime => item.kind === 'runtime'),
      machines: rosterResources.filter((item): item is Machine => item.kind === 'machine'),
      order: rosterResources.filter((item) => item.kind === 'agent').map((item) => item.id),
      recorded_history: rosterHistory,
    }, timeline: roster },
    details: { ...common, kind: 'details', state: { missions: [], work: [] }, timeline: [] },
    attention: { ...common, kind: 'attention', state: { attention: [], messages: [] }, timeline: [] },
    conversation: { ...common, kind: 'conversation', state: { threads, recorded_history: conversationHistory }, timeline: conversation },
    terminal: { ...common, kind: 'terminal', state: { terminals, recorded_history: screenHistory }, timeline: terminal },
    sync: { ...common, kind: 'sync', state: { expected: [] }, timeline: [] },
  }, recording: { frames, http, socket_ids: [...new Set(rows.filter((row) => row.kind === 'command').flatMap((row) => row.socket_id === null ? [] : [row.socket_id]))] } }
}

/** Shift schema-declared instants only: opaque tool input, output and text stay byte-identical. */
const rebaseValue = (raw: JsonObject, value: JsonValue, shift: number): JsonValue => {
  const transformed = new Set<string>()
  const walk = (raw: JsonObject, value: JsonValue, pointer: string): JsonValue => {
    const schema = resolve(raw)
    if (transformed.has(pointer)) return value
    if (schema['x-st-codec'] === 'timestamp' && typeof value === 'string') {
      transformed.add(pointer)
      return formatTimestamp(parseTimestamp(value) + shift)
    }
    if (schema['x-st-codec'] === 'epoch-ms' && typeof value === 'number') {
      transformed.add(pointer)
      return value + shift
    }
    let result = value
    for (const key of ['allOf', 'oneOf', 'anyOf']) {
      const branches = schema[key]
      if (Array.isArray(branches)) for (const branch of branches) {
        if (key === 'allOf' || matches(object(branch), value)) result = walk(object(branch), result, pointer)
      }
    }
    if (schema.if !== undefined && schema.then !== undefined && matches(object(schema.if), value)) result = walk(object(schema.then), result, pointer)
    if (Array.isArray(result) && schema.items !== undefined) return result.map((item, index) => walk(object(schema.items), item, `${pointer}/${index}`))
    if (result !== null && typeof result === 'object' && !Array.isArray(result)) {
      const properties = schema.properties === undefined ? {} : object(schema.properties)
      return Object.fromEntries(Object.entries(result).map(([key, item]) => [
        key, properties[key] === undefined ? item : walk(object(properties[key]), item, `${pointer}/${key}`),
      ]))
    }
    return result
  }
  return walk(raw, value, '')
}

/** Effect/filesystem-free entry point for statically imported normalized recording JSON. */
export const importNormalizedRecording = (scriptInput: unknown, rowsInput: unknown, now = ANCHOR_MS): { slices: Slices; recording: RecordedTransport } => {
  const script = object(json(scriptInput, '/script'))
  const version = integer(script.format_version)
  if (version !== 1 && version !== 2) fail('/script/format_version', 'unsupported format')
  const id_map = Object.fromEntries(Object.entries(object(script.id_map)).map(([key, value]) => [key, text(value)]))
  const kinds: readonly RecordingRow['kind'][] = ['header', 'http', 'attach', 'command', 'frame', 'baseline', 'harness', 'pty_output']
  const isKind = (value: string): value is RecordingRow['kind'] => kinds.some((kind) => kind === value)
  const shift = nowMs(now) - ANCHOR_MS
  const rows = array(json(rowsInput, '/rows')).map((input): RecordingRow => {
    const row = object(input)
    const kind = text(row.kind)
    if (!isKind(kind)) fail('/rows/kind', 'unsupported row')
    let value = object(row.value)
    if (shift !== 0 && kind === 'frame') value = object(rebaseValue(node('CollectionFrame'), value, shift))
    if (shift !== 0 && kind === 'http') {
      value = { ...value, body: rebaseValue(node(integer(value.status) < 400 ? 'Envelope' : 'ErrorEnvelope'), value.body ?? fail('/body', 'missing body'), shift) }
    }
    return { kind, ordinal: integer(row.ordinal), receive_ms: integer(row.receive_ms), socket_id: row.socket_id === null ? null : text(row.socket_id), value }
  })
  return importRecording({ format_version: version, world: text(script.world), agent: text(script.agent), host: text(script.host), id_map }, rows)
}

const importedTimes = new WeakMap<Partial<Slices>, readonly TimePointer[]>()

/** Rebase already imported anchor slices; schema traversal is cached, never repeated per load. */
export const rebaseImportedSlices = <TSlices extends Partial<Slices>>(slices: TSlices, now: number): TSlices => {
  if (nowMs(now) === ANCHOR_MS) return slices
  let times = importedTimes.get(slices)
  if (times === undefined) {
    const found = new Map<string, TimePointer>()
    const visit = (raw: JsonObject, value: JsonValue, pointer: string): void => {
      const schema = resolve(raw)
      const codec = schema['x-st-codec']
      if (codec === 'timestamp' || codec === 'epoch-ms') {
        found.set(pointer, { pointer, codec })
        return
      }
      for (const key of ['allOf', 'oneOf', 'anyOf']) {
        const branches = schema[key]
        if (Array.isArray(branches)) for (const branch of branches) {
          if (key === 'allOf' || matches(object(branch), value)) visit(object(branch), value, pointer)
        }
      }
      if (schema.if !== undefined && schema.then !== undefined && matches(object(schema.if), value)) visit(object(schema.then), value, pointer)
      if (Array.isArray(value) && schema.items !== undefined) {
        value.forEach((item, index) => visit(object(schema.items), item, `${pointer}/${index}`))
      } else if (value !== null && typeof value === 'object' && !Array.isArray(value)) {
        const properties = schema.properties === undefined ? {} : object(schema.properties)
        for (const [key, item] of Object.entries(value)) {
          if (properties[key] !== undefined) visit(object(properties[key]), item, `${pointer}/${escapePointerToken(key)}`)
        }
      }
    }
    for (const kind of SLICE_KINDS) {
      const slice = slices[kind]
      if (slice === undefined) continue
      for (const wire of wireValues(slice)) visit(node(wire.definition), json(wire.value, wire.pointer), `/${kind}${wire.pointer}`)
    }
    times = [...found.values()]
    importedTimes.set(slices, times)
  }
  return rebase(slices, times, ANCHOR, nowMs(now))
}
