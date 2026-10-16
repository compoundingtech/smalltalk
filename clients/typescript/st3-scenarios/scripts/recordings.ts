import { readFileSync } from 'node:fs'
import * as Schema from '@smalltalk/st3-client/schema'
import type { Runtime, Snapshot, TerminalAttachment, TerminalScreen, TimelineEntry } from '@smalltalk/st3-client'
import type { RecordedFrame, RecordedHttp, RecordedTransport } from '../src/kit/recording.ts'
import type { ConversationEvent, ConversationThread, TerminalEvent, TerminalRecord } from '../src/kit/slice.ts'
import type { Slices } from '../src/kit/world.ts'
import { SCHEMA_PATH } from './times.ts'
import { assertUniqueJsonMembers } from './unique-json.ts'

export type JsonValue = null | boolean | number | string | JsonValue[] | JsonObject
export interface JsonObject { [key: string]: JsonValue }
export interface RecordingRow {
  readonly ordinal: number
  readonly socket_id: string | null
  readonly receive_ms: number
  readonly kind: 'header' | 'http' | 'attach' | 'command' | 'frame' | 'baseline' | 'harness' | 'pty_output'
  readonly value: JsonObject
}
export interface RecordingScript {
  readonly format_version: 1
  readonly world: string
  readonly agent: string
  readonly host: string
  readonly person: string
  readonly workspace: string
  readonly id_map: Readonly<Record<string, string>>
  readonly turns: readonly { readonly prompt: string; readonly answer: string; readonly timestamp: string; readonly command?: string }[]
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
const keys = (value: JsonObject, allowed: readonly string[], pointer: string): void => {
  for (const key of Object.keys(value)) if (!allowed.includes(key)) fail(`${pointer}/${key}`, 'unexpected field')
}
const instant = (value: JsonValue | undefined, pointer: string): number => {
  const s = text(value, pointer)
  if (!/^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?(?:Z|[+-]\d\d:\d\d)$/u.test(s) || !Number.isFinite(Date.parse(s))) fail(pointer, 'invalid instant')
  return Date.parse(s)
}
const scriptSchema = object(json(JSON.parse(readFileSync(new URL('./recording-script.schema.json', import.meta.url), 'utf8')), ''))
const scriptKeys = Object.keys(object(scriptSchema.properties))
const turnKeys = Object.keys(object(object(object(object(scriptSchema.properties).turns).items).properties))

export const decodeScript = (input: unknown): RecordingScript => {
  const s = object(json(input, ''), '')
  keys(s, scriptKeys, '')
  if (s.format_version !== 1) fail('/format_version', 'unsupported version')
  const ids = object(s.id_map, '/id_map')
  const id_map: Record<string, string> = {}
  for (const [key, value] of Object.entries(ids)) id_map[key] = text(value, `/id_map/${key}`)
  return { format_version: 1, world: text(s.world, '/world'), agent: text(s.agent, '/agent'), host: text(s.host, '/host'), person: text(s.person, '/person'), workspace: text(s.workspace, '/workspace'), id_map,
    turns: array(s.turns, '/turns').map((value, index) => {
      const t = object(value, `/turns/${index}`)
      keys(t, turnKeys, `/turns/${index}`)
      instant(t.timestamp, `/turns/${index}/timestamp`)
      return { prompt: text(t.prompt), answer: text(t.answer), timestamp: text(t.timestamp), ...(t.command === undefined ? {} : { command: text(t.command) }) }
    }), reads: array(s.reads, '/reads').map((value, i) => text(value, `/reads/${i}`)) }
}
const kinds = ['header', 'http', 'attach', 'command', 'frame', 'baseline', 'harness', 'pty_output'] as const
const isKind = (value: string): value is RecordingRow['kind'] => kinds.some((kind) => kind === value)
export const decodeRecording = (source: string): RecordingRow[] => {
  let values: unknown[]
  try {
    const trimmed = source.trim()
    if (trimmed.startsWith('[')) {
      assertUniqueJsonMembers(trimmed)
      const parsed: unknown = JSON.parse(trimmed)
      if (!Array.isArray(parsed)) return fail('', 'expected rows')
      values = parsed
    } else values = trimmed.split('\n').map((line, i) => {
      try { assertUniqueJsonMembers(line); return JSON.parse(line) }
      catch (error) { return fail(`/${i}`, error instanceof Error ? error.message : 'invalid JSON') }
    })
  } catch (error) { return fail('', error instanceof Error ? error.message : 'invalid JSON') }
  let previous = -1
  let previousReceive = -Infinity
  return values.map((input, index) => {
    const p = `/${index}`
    const row = object(json(input, p), p)
    keys(row, ['ordinal', 'socket_id', 'receive_ms', 'kind', 'value'], p)
    const ordinal = integer(row.ordinal, `${p}/ordinal`)
    if (ordinal <= previous) fail(`${p}/ordinal`, 'ordinals must strictly increase')
    previous = ordinal
    const kind = text(row.kind, `${p}/kind`)
    if (!isKind(kind)) return fail(`${p}/kind`, 'unknown row kind')
    const socket_id = row.socket_id === null ? null : text(row.socket_id, `${p}/socket_id`)
    if ((kind === 'command' || kind === 'frame') && socket_id === null) fail(`${p}/socket_id`, 'socket required')
    if (kind !== 'command' && kind !== 'frame' && socket_id !== null) fail(`${p}/socket_id`, 'non-socket row must use null')
    const receive_ms = integer(row.receive_ms, `${p}/receive_ms`)
    if (receive_ms < previousReceive) fail(`${p}/receive_ms`, 'receive times must not decrease')
    previousReceive = receive_ms
    return { ordinal, socket_id, receive_ms, kind, value: object(row.value, `${p}/value`) }
  })
}
const definitions = object(object(json(JSON.parse(readFileSync(SCHEMA_PATH, 'utf8')), '')).$defs)
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
  }
  if (Array.isArray(s.allOf) && !s.allOf.every((branch) => matches(object(branch), value))) return false
  for (const key of ['oneOf', 'anyOf']) if (Array.isArray(s[key]) && !s[key].some((branch) => matches(object(branch), value))) return false
  return true
}

/** Schema-directed N: opaque object payloads and strings have no inferred meaning. */
export const normalizeRecording = (script: RecordingScript, rows: readonly RecordingRow[], anchorMs: number): RecordingRow[] => {
  const header = rows.find((row) => row.kind === 'header') ?? fail('/header', 'missing header')
  const baseline = rows.find((row) => row.kind === 'baseline') ?? fail('/baseline', 'missing baseline')
  if (rows.filter((r) => r.kind === 'header').length !== 1 || rows.filter((r) => r.kind === 'baseline').length !== 1) fail('', 'duplicate header or baseline')
  if (header.value.format_version !== 1 || header.value.script_format_version !== 1 || header.value.world !== script.world) fail('/header/value', 'script/header mismatch')
  keys(header.value, ['format_version', 'script_format_version', 'world', 'daemon_build', 'daemon_sha256', 'started_at'], '/header/value')
  text(header.value.daemon_build, '/header/value/daemon_build')
  if (!/^[0-9a-f]{64}$/u.test(text(header.value.daemon_sha256, '/header/value/daemon_sha256'))) fail('/header/value/daemon_sha256', 'invalid SHA256')
  keys(baseline.value, ['last_pre_timeline_ordinal'], '/baseline/value')
  integer(baseline.value.last_pre_timeline_ordinal, '/baseline/value/last_pre_timeline_ordinal')
  const witness = rows.find((row) => row.ordinal === baseline.value.last_pre_timeline_ordinal) ?? fail('/baseline/value/last_pre_timeline_ordinal', 'unknown baseline ordinal')
  if (witness.ordinal >= baseline.ordinal || witness.kind !== 'frame') fail('/baseline/value/last_pre_timeline_ordinal', 'baseline must witness a prior frame')
  const origin = instant(header.value.started_at, '/header/value/started_at')
  const normalized = witness.receive_ms === 0
  const shift = normalized ? 0 : anchorMs - (origin + witness.receive_ms)
  const stores = new Set<number>()
  const snapshots = new Map<string, { host: string; store: JsonValue }>()
  const collect = (value: JsonValue, pointer: string): void => {
    if (value === null || typeof value !== 'object' || Array.isArray(value)) fail(pointer, 'expected snapshot')
    if (value.store_index !== undefined && value.host_id !== undefined && value.id !== undefined) {
      const host = text(value.host_id, `${pointer}/host_id`)
      const id = text(value.id, `${pointer}/id`)
      if (host !== script.host && host !== script.id_map[script.host]) fail(`${pointer}/host_id`, 'snapshot host mismatch')
      const store = value.store_index
      if (typeof store === 'number') { if (!Number.isInteger(store) || store < 0) fail(`${pointer}/store_index`, 'invalid store'); stores.add(store) }
      else if (typeof store !== 'string' || !/^store#[1-9]\d*$/u.test(store)) fail(`${pointer}/store_index`, 'invalid store token')
      const old = snapshots.get(id)
      if (old && (old.host !== host || old.store !== store)) fail(`${pointer}/id`, 'snapshot equality mismatch')
      snapshots.set(id, { host, store })
    }
  }
  rows.forEach((row, i) => {
    if (row.kind === 'frame' && row.value.snapshot !== undefined) collect(row.value.snapshot, `/${i}/value/snapshot`)
    if (row.kind === 'http') {
      const envelope = object(row.value.body, `/${i}/value/body`)
      if (envelope.snapshot !== undefined) collect(envelope.snapshot, `/${i}/value/body/snapshot`)
    }
  })
  const storeMap = new Map([...stores].sort((a, b) => a - b).map((store, i) => [store, `store#${normalized ? store : i + 1}`]))
  const tokens = new Map<string, Map<string, string>>()
  const token = (kind: string, value: string): string => {
    if (new RegExp(`^${kind}#[1-9]\\d*$`, 'u').test(value)) return value
    let map = tokens.get(kind)
    if (!map) { map = new Map(); tokens.set(kind, map) }
    let out = map.get(value)
    if (!out) { out = `${kind}#${map.size + 1}`; map.set(value, out) }
    return out
  }
  const identity = (value: string, pointer: string): string => script.id_map[value] ?? (Object.values(script.id_map).includes(value) ? value : fail(pointer, `unknown identity ${value}`))
  // allOf/conditional schema paths can declare the same leaf more than once.
  const transformed = new Set<string>()
  const walk = (raw: JsonObject, value: JsonValue, pointer: string, key = ''): JsonValue => {
    if ((value === null || typeof value !== 'object') && transformed.has(pointer)) return value
    const leaf = (result: JsonValue): JsonValue => { transformed.add(pointer); return result }
    const s = resolve(raw)
    const ref = typeof raw.$ref === 'string' ? raw.$ref.split('/').at(-1) : undefined
    if (value !== null && typeof value === 'string') {
      if (s['x-st-codec'] === 'timestamp') return leaf(new Date(instant(value, pointer) + shift).toISOString())
      if (ref === 'SnapshotId' || key === 'snapshot_id') {
        const snapshot = snapshots.get(value)
        if (snapshot) return leaf(`snapshot/${identity(snapshot.host, `${pointer}/host`)}/${typeof snapshot.store === 'number' ? storeMap.get(snapshot.store) : snapshot.store}`)
        return fail(pointer, `unknown snapshot ${value}`)
      }
      if (ref === 'RequestId' || key === 'request_id') return leaf(token('request', value))
      if (key === 'operation_id') return leaf(token('operation', value))
      if (key === 'attachment_id' || (ref === 'Id' && value.startsWith('terminal-attachment/'))) return leaf(token('attachment', value))
      if (key === 'stream_capability' || key === 'capability') return leaf(token('capability', value))
      if (key === 'runtime_incarnation' || key === 'incarnation') return leaf(token('incarnation', value))
      if (key === 'next_cursor' || key === 'cursor') return leaf(token('cursor', value))
      if (key === 'stream_url') {
        const url = new URL(value, 'http://scenario.invalid')
        for (const [from, to] of Object.entries(script.id_map)) url.pathname = url.pathname.replaceAll(encodeURIComponent(from), encodeURIComponent(to))
        const incarnation = url.searchParams.get('incarnation')
        if (incarnation !== null) url.searchParams.set('incarnation', token('incarnation', incarnation))
        return leaf(`${url.pathname}${url.search}`)
      }
      if (s['x-st-ref'] !== undefined || ref?.endsWith('Id') || key === 'message_id' || key === 'reply_to') {
        if (/^(?:attachment|operation)#[1-9]\d*$/u.test(value)) return leaf(value)
        return leaf(identity(value, pointer))
      }
    }
    if (key === 'store_index' && typeof value === 'number') return leaf(storeMap.get(value) ?? fail(pointer, 'unknown store'))
    if (!matches(s, value) && !(key === 'store_index' && typeof value === 'string')) fail(pointer, 'malformed schema value')
    if (s['x-st-codec'] === 'epoch-ms' && typeof value === 'number') return leaf(Math.trunc(value + shift))
    let result = value
    for (const branchKey of ['allOf', 'oneOf', 'anyOf']) {
      const branches = s[branchKey]
      if (Array.isArray(branches)) for (const branch of branches) if (branchKey === 'allOf' || matches(object(branch), value)) result = walk(object(branch), result, pointer, key)
    }
    if (s.if !== undefined && matches(object(s.if), value) && s.then !== undefined) result = walk(object(s.then), result, pointer, key)
    if (Array.isArray(result) && s.items !== undefined) return result.map((item, i) => walk(object(s.items), item, `${pointer}/${i}`))
    if (result !== null && typeof result === 'object' && !Array.isArray(result)) {
      const out: JsonObject = { ...result }
      const properties = s.properties === undefined ? {} : object(s.properties)
      for (const [name, item] of Object.entries(result)) {
        if (properties[name] !== undefined) out[name] = walk(object(properties[name]), item, `${pointer}/${name}`, name)
        else if (s.additionalProperties === false) fail(`${pointer}/${name}`, 'unexpected field')
      }
      return out
    }
    return result
  }
  return rows.map((row, i) => {
    const p = `/${i}/value`
    let value = structuredClone(row.value)
    if (row.kind === 'frame') value = object(walk(node('CollectionFrame'), value, p))
    if (row.kind === 'command') value = object(walk(node('CollectionCommand'), value, p))
    if (row.kind === 'attach') value = object(walk(node('ActionRequest'), value, p))
    if (row.kind === 'http') {
      keys(value, ['method', 'path', 'query', 'status', 'request_body', 'body'], p)
      text(value.method, `${p}/method`); text(value.path, `${p}/path`); text(value.query, `${p}/query`)
      const status = integer(value.status, `${p}/status`)
      if (status < 100 || status > 599) fail(`${p}/status`, 'invalid status')
      value.body = walk(node(status < 400 ? 'Envelope' : 'ErrorEnvelope'), value.body ?? fail(`${p}/body`, 'missing body'), `${p}/body`)
      if (value.request_body !== null) value.request_body = walk(node('ActionRequest'), value.request_body ?? fail(`${p}/request_body`, 'missing body'), `${p}/request_body`)
      let path = text(value.path)
      const terminalPath = /^\/v1\/client\/terminals\/([^/]+)\/(screen|stream)$/u.exec(path)
      const timelinePath = /^\/v1\/client\/sessions\/(.+)\/timeline$/u.exec(path)
      if (terminalPath) path = `/v1/client/terminals/${encodeURIComponent(identity(decodeURIComponent(terminalPath[1]!), `${p}/path`))}/${terminalPath[2]}`
      if (timelinePath) path = `/v1/client/sessions/${identity(decodeURIComponent(timelinePath[1]!), `${p}/path`)}/timeline`
      value.path = path
      const query = new URLSearchParams(text(value.query))
      if (query.has('cursor')) query.set('cursor', token('cursor', query.get('cursor')!))
      value.query = query.toString()
    }
    if (row.kind === 'header') value.started_at = new Date(origin + shift).toISOString()
    if (row.kind === 'harness') {
      keys(value, ['turn', 'prompt', 'answer', 'timestamp', 'command'], p)
      integer(value.turn, `${p}/turn`); text(value.prompt, `${p}/prompt`); text(value.answer, `${p}/answer`)
      if (value.command !== null) text(value.command, `${p}/command`)
      value.timestamp = new Date(instant(value.timestamp, `${p}/timestamp`) + shift).toISOString()
    }
    if (row.kind === 'pty_output') {
      keys(value, ['turn', 'terminal', 'text'], p)
      integer(value.turn, `${p}/turn`); text(value.text, `${p}/text`)
      value.terminal = identity(text(value.terminal, `${p}/terminal`), `${p}/terminal`)
    }
    return { ...row, receive_ms: normalized ? row.receive_ms : row.receive_ms - witness.receive_ms, value }
  })
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
const snapshotWire = (value: unknown): value is Snapshot => {
  Schema.decodeUnknownSync(Schema.Snapshot, 'strict')(value)
  return legacyShape(node('Snapshot'), json(value, '/snapshot'))
}
const terminalScreenWire = (value: unknown): value is TerminalScreen => {
  Schema.decodeUnknownSync(Schema.TerminalScreen, 'strict')(value)
  return legacyShape(node('TerminalScreen'), json(value, '/screen'))
}
const timelineEntryWire = (value: unknown): value is TimelineEntry => {
  Schema.decodeUnknownSync(Schema.TimelineEntry, 'strict')(value)
  return legacyShape(node('TimelineEntry'), json(value, '/entry'))
}
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
export const importRecording = (script: RecordingScript, rows: readonly RecordingRow[]): { slices: Slices; recording: RecordedTransport } => {
  const source = { _tag: 'recorded', recording: `${script.world}-proof` } as const
  const common = { variant: 'default', source, decode: 'strict', loading: false } as const
  const frames: RecordedFrame[] = []
  const http: RecordedHttp[] = []
  const conversation: ConversationEvent[] = []
  const terminal: TerminalEvent[] = []
  const threads: ConversationThread[] = []
  const terminals: TerminalRecord[] = []
  const agent = script.id_map[script.agent] ?? script.agent
  const host = script.id_map[script.host] ?? script.host
  let store = 1
  let attachment: TerminalAttachment | undefined
  for (const row of rows) {
    const v = row.value
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
      } else if (value.kind === 'terminal-screen') http.push({ ...metadata, value: { kind: 'screen', terminal: text(value.terminal_id) } })
      else {
        const p = object(value.page)
        const page = { limit: integer(p.limit), has_more: bool(p.has_more), ...(p.next_cursor === undefined ? {} : { next_cursor: text(p.next_cursor) }) }
        if (value.kind === 'timeline-page') {
          const items = array(value.items).map((item) => {
            const entry = object(item)
            return { id: text(entry.id), blocks: object(entry.body).blocks !== undefined }
          })
          http.push({ ...metadata, value: { kind: 'timeline', agent, items, page } })
        }
        else if (value.kind === 'page' && value.collection === 'agents') {
          if (array(value.items).length !== 0) fail(`/ordinal/${row.ordinal}/value/body/value/items`, 'agent resource capture not supported by this proof')
          const filters: Record<string, string> = {}
          for (const [key, item] of Object.entries(object(value.filters))) filters[key] = text(item)
          http.push({ ...metadata, value: { kind: 'agents', page, filters } })
        } else fail(`/ordinal/${row.ordinal}`, 'unsupported HTTP capture')
      }
    }
    if (row.kind !== 'frame') continue
    if (v.kind === 'conversation') {
      const items = entries(v.items)
      const replace = bool(v.replace)
      const selector = { collection: 'conversation', conversation: agent } as const
      const boundary = { ordinal: row.ordinal, at_ms: row.receive_ms, selector, kind: 'conversation', replace, ...(v.has_more === undefined ? {} : { has_more: bool(v.has_more) }) } as const
      if (row.receive_ms <= 0) {
        threads.splice(0, threads.length, { agent, session_id: text(v.session_id), items, page_size: 50, has_more: v.has_more === undefined ? false : bool(v.has_more) })
        frames.push(boundary)
      } else {
        const event = conversation.length
        conversation.push(replace ? { _tag: 'replace', at_ms: row.receive_ms, store, agent, session_id: text(v.session_id), items, has_more: v.has_more === undefined ? false : bool(v.has_more) } : { _tag: 'entries', at_ms: row.receive_ms, store, agent, items })
        frames.push({ ...boundary, event })
      }
    } else if (v.kind === 'screen') {
      const snapshot = numericSnapshot(v.snapshot)
      store = snapshot.store_index
      const screen = screenValue(v.value)
      const selector = { collection: 'terminal', terminal: screen.terminal_id } as const
      if (row.receive_ms <= 0) {
        // No runtime resource was captured. This descriptive adapter contains only the screen's
        // ownership/incarnation and required unknown values, not invented process/launch facts.
        const runtime: Runtime = { id: `runtime/${screen.terminal_id}`, kind: 'runtime', revision: screen.revision, updated_at: snapshot.created_at, desired_revision: null, incarnation_id: screen.runtime_incarnation, owner_host_id: host, owner_id: agent, runtime_id: screen.terminal_id, runtime_kind: 'terminal', state: 'running', terminal_id: screen.terminal_id }
        const pastScreens = rows.filter((r) => r.kind === 'http' && r.receive_ms <= 0 && object(r.value.body).value !== undefined && object(object(r.value.body).value).kind === 'terminal-screen')
          .map((r) => ({ at_ms: r.receive_ms, screen: screenValue(object(r.value.body).value) }))
        terminals.push({ terminal: screen.terminal_id, owner: agent, runtime, incarnation: screen.runtime_incarnation, cast: { header: { version: 2, width: screen.columns, height: screen.rows, ...(screen.title === undefined ? {} : { title: screen.title }) }, started_at_ms: 0, events: rows.filter((r) => r.kind === 'pty_output').map((r): [number, 'o', string] => [r.receive_ms / 1000, 'o', text(r.value.text)]) }, screens: [...pastScreens, { at_ms: row.receive_ms, screen }] })
        frames.push({ ordinal: row.ordinal, at_ms: row.receive_ms, selector, kind: 'screen', snapshot })
      } else {
        const event = terminal.length
        terminal.push({ _tag: 'screen', terminal: screen.terminal_id, screen, at_ms: row.receive_ms, store })
        frames.push({ ordinal: row.ordinal, at_ms: row.receive_ms, selector, kind: 'screen', snapshot, event })
      }
    } else fail(`/ordinal/${row.ordinal}`, 'unsupported frame capture')
  }
  if (terminals.length === 0 || !attachment) fail('/terminal', 'missing captured screen or attachment')
  return { slices: {
    roster: { ...common, kind: 'roster', state: { agents: [], runtimes: [], machines: [], order: [] }, timeline: [] },
    details: { ...common, kind: 'details', state: { missions: [], work: [] }, timeline: [] },
    attention: { ...common, kind: 'attention', state: { attention: [], messages: [] }, timeline: [] },
    conversation: { ...common, kind: 'conversation', state: { threads }, timeline: conversation },
    terminal: { ...common, kind: 'terminal', state: { terminals }, timeline: terminal },
    sync: { ...common, kind: 'sync', state: { expected: [] }, timeline: [] },
  }, recording: { frames, http } }
}
