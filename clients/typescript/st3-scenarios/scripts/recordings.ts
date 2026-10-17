import { readFileSync } from 'node:fs'
import * as Schema from '@smalltalk/st3-client/schema'
import type { RecordingRow, RecordingScript, RecordingTurn, JsonValue, JsonObject } from '../src/kit/importRecording.ts'
export { importRecording } from '../src/kit/importRecording.ts'
export type { RecordingRow, RecordingScript, RecordingTurn, JsonValue, JsonObject } from '../src/kit/importRecording.ts'
import { SCHEMA_PATH } from './times.ts'
import { assertUniqueJsonMembers } from './unique-json.ts'

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
  if (s.format_version !== 1 && s.format_version !== 2) fail('/format_version', 'unsupported version')
  const ids = object(s.id_map, '/id_map')
  const id_map: Record<string, string> = {}
  for (const [key, value] of Object.entries(ids)) id_map[key] = text(value, `/id_map/${key}`)
  const agent = text(s.agent, '/agent')
  const host = text(s.host, '/host')
  const agents = s.agents === undefined ? undefined : array(s.agents, '/agents').map((value, index) => {
    const p = `/agents/${index}`
    const a = object(value, p)
    keys(a, ['agent', 'host'], p)
    const member = { agent: text(a.agent, `${p}/agent`), host: text(a.host, `${p}/host`) }
    if (member.host !== host) fail(`${p}/host`, 'recorded agents must belong to the primary host')
    return member
  })
  if (agents && new Set(agents.map((a) => a.agent)).size !== agents.length) fail('/agents', 'duplicate agent')
  const turns = array(s.turns, '/turns').map((value, index): RecordingTurn => {
    const p = `/turns/${index}`
    const t = object(value, p)
    keys(t, turnKeys, p)
    instant(t.timestamp, `${p}/timestamp`)
    const owner = t.agent === undefined ? agent : text(t.agent, `${p}/agent`)
    if (owner !== agent && !agents?.some((a) => a.agent === owner)) fail(`${p}/agent`, 'unknown recorded agent')
    if (t.command !== undefined && owner !== agent) fail(`${p}/command`, 'terminal commands require the primary agent')
    let tool: RecordingTurn['tool']
    if (t.tool !== undefined) {
      const value = object(t.tool, `${p}/tool`)
      keys(value, ['id', 'name', 'input', 'result'], `${p}/tool`)
      if (value.input === undefined || value.result === undefined) fail(`${p}/tool`, 'input and result are required JSON values')
      tool = { id: text(value.id, `${p}/tool/id`), name: text(value.name, `${p}/tool/name`), input: value.input, result: value.result }
    }
    return { prompt: text(t.prompt, `${p}/prompt`), answer: text(t.answer, `${p}/answer`), timestamp: text(t.timestamp),
      ...(t.agent === undefined ? {} : { agent: owner }), ...(t.command === undefined ? {} : { command: text(t.command) }),
      ...(tool === undefined ? {} : { tool }),
      ...(t.revisions === undefined ? {} : { revisions: array(t.revisions, `${p}/revisions`).map((value, index) => text(value, `${p}/revisions/${index}`)) }) }
  })
  const nonnegative = (value: JsonValue | undefined, pointer: string): number => {
    const n = integer(value, pointer)
    return n >= 0 ? n : fail(pointer, 'expected nonnegative integer')
  }
  const historical_turns = s.historical_turns === undefined ? undefined : nonnegative(s.historical_turns, '/historical_turns')
  if (historical_turns !== undefined && historical_turns > turns.length) fail('/historical_turns', 'exceeds turn count')
  const reconnect_after = s.reconnect_after === undefined ? undefined : array(s.reconnect_after, '/reconnect_after').map((value, i) => {
    const n = nonnegative(value, `/reconnect_after/${i}`)
    if (n >= turns.length || n < (historical_turns ?? 0)) fail(`/reconnect_after/${i}`, 'requires a future turn index')
    return n
  })
  if (reconnect_after && new Set(reconnect_after).size !== reconnect_after.length) fail('/reconnect_after', 'duplicate turn index')
  if (s.format_version === 2 && s.anchor === undefined) fail('/anchor', 'version 2 requires an authored anchor')
  if (s.anchor !== undefined) instant(s.anchor, '/anchor')
  return { format_version: s.format_version, world: text(s.world, '/world'), agent, host, person: text(s.person, '/person'), workspace: text(s.workspace, '/workspace'), id_map, turns,
    reads: array(s.reads, '/reads').map((value, i) => text(value, `/reads/${i}`)),
    ...(s.anchor === undefined ? {} : { anchor: text(s.anchor, '/anchor') }),
    ...(agents === undefined ? {} : { agents }), ...(historical_turns === undefined ? {} : { historical_turns }),
    ...(s.empty_baseline === undefined ? {} : { empty_baseline: bool(s.empty_baseline, '/empty_baseline') }),
    ...(s.walk_pages === undefined ? {} : { walk_pages: bool(s.walk_pages, '/walk_pages') }),
    ...(reconnect_after === undefined ? {} : { reconnect_after }) }
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
export const normalizeRecording = (script: RecordingScript, rows: readonly RecordingRow[], anchorMs: number): RecordingRow[] =>
  normalizeWithIdentity(script, rows, anchorMs)

/** Collect only schema-annotated identities, never opaque tool input/output strings. */
export const recordingIdentities = (script: RecordingScript, rows: readonly RecordingRow[]): readonly string[] => {
  const identities = new Set<string>()
  normalizeWithIdentity(script, rows, 0, (value) => { identities.add(value); return value })
  return [...identities]
}

const normalizeWithIdentity = (script: RecordingScript, rows: readonly RecordingRow[], anchorMs: number, mapIdentity?: (value: string) => string): RecordingRow[] => {
  const header = rows.find((row) => row.kind === 'header') ?? fail('/header', 'missing header')
  const baseline = rows.find((row) => row.kind === 'baseline') ?? fail('/baseline', 'missing baseline')
  if (rows.filter((r) => r.kind === 'header').length !== 1 || rows.filter((r) => r.kind === 'baseline').length !== 1) fail('', 'duplicate header or baseline')
  if (header.value.format_version !== 1 || header.value.script_format_version !== script.format_version || header.value.world !== script.world) fail('/header/value', 'script/header mismatch')
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
  // An action result can name its later commit fence while the HTTP envelope still
  // carries the earlier read fence. Derive only that typed reference's host/store,
  // never a timestamp or a snapshot object, from the daemon's SnapshotId encoding.
  rows.forEach((row, i) => {
    if (row.kind !== 'http') return
    const envelope = object(row.value.body)
    if (envelope.value === undefined) return
    const value = object(envelope.value)
    if (value.kind !== 'action-result' || value.snapshot_id === undefined) return
    const pointer = `/${i}/value/body/value/snapshot_id`
    const id = text(value.snapshot_id, pointer)
    if (snapshots.has(id)) return
    const raw = /^snapshot\/([^/]+)\/(\d+)\/[0-9a-f]{16}$/u.exec(id)
    const token = normalized ? /^snapshot\/(host\/[^/]+)\/(store#[1-9]\d*)$/u.exec(id) : null
    const host = raw?.[1] === undefined ? token?.[1] : 'host/' + raw[1]
    if (host === undefined || host !== script.host && host !== script.id_map[script.host]) fail(pointer, 'snapshot host mismatch')
    const store = raw?.[2] === undefined ? token?.[2] : Number(raw[2])
    if (typeof store === 'number') {
      if (!Number.isSafeInteger(store) || store < 0) fail(pointer, 'invalid snapshot store')
      stores.add(store)
    } else if (store === undefined) fail(pointer, 'unknown snapshot ' + id)
    snapshots.set(id, { host, store })
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
  const identity = (value: string, pointer: string): string => mapIdentity ? mapIdentity(value) : script.id_map[value] ?? (Object.values(script.id_map).includes(value) ? value : fail(pointer, `unknown identity ${value}`))
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
    if (row.kind === 'command' && value.kind === 'disconnect') keys(value, ['kind'], p)
    if (row.kind === 'command' && value.kind !== 'disconnect') value = object(walk(node('CollectionCommand'), value, p))
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
      keys(value, ['turn', 'prompt', 'answer', 'timestamp', 'command', ...(script.format_version === 2 ? ['agent', 'tool', 'revisions'] : [])], p)
      integer(value.turn, `${p}/turn`); text(value.prompt, `${p}/prompt`); text(value.answer, `${p}/answer`)
      if (value.command !== null) text(value.command, `${p}/command`)
      if (script.format_version === 2 && value.agent !== undefined) value.agent = identity(text(value.agent, `${p}/agent`), `${p}/agent`)
      if (value.revisions !== undefined) array(value.revisions, `${p}/revisions`).forEach((item, index) => text(item, `${p}/revisions/${index}`))
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

