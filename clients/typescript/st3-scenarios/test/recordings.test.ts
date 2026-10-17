import { readFileSync } from 'node:fs'
import { describe, expect, it } from 'vitest'
import { decodeRecording, decodeScript, normalizeRecording, type JsonObject, type JsonValue, type RecordingRow, type RecordingScript } from '../scripts/recordings.ts'

const anchor = Date.parse('2026-01-01T00:00:00Z')
const script: RecordingScript = decodeScript({ format_version: 1, world: 'proof', agent: 'agent/example/a', host: 'host/meadow', person: 'person/ada', workspace: '~/src/example', turns: [], reads: [], id_map: { 'host/meadow': 'host/meadow', 'agent/example/a': 'agent/example/a', 'session/raw': 'session/proof', 'timeline-entry/raw': 'timeline-entry/proof' } })
const snapshot = (store: number, id = `snapshot/meadow/${store}/opaque`): JsonObject => ({ id, host_id: 'host/meadow', store_index: store, projection_version: 'client-projection.v0', created_at: '2026-10-13T03:07:43.010123Z' })
const rows = (): RecordingRow[] => [
  { ordinal: 0, socket_id: null, receive_ms: 26096, kind: 'header', value: { format_version: 1, script_format_version: 1, world: 'proof', started_at: '2026-10-13T03:07:12.491455617Z', daemon_build: 'fixture', daemon_sha256: 'a'.repeat(64) } },
  { ordinal: 1, socket_id: null, receive_ms: 30491, kind: 'http', value: { method: 'GET', path: '/v1/client/agents', query: '', status: 200, request_body: null, body: { api_version: 'st3.client.v0', request_id: 'request/raw', snapshot: snapshot(9), value: { kind: 'page', collection: 'agents', filters: {}, items: [], page: { limit: 50, has_more: false, next_cursor: 'opaque-cursor' } } } } },
  { ordinal: 7, socket_id: 'socket#1', receive_ms: 30543, kind: 'frame', value: { kind: 'conversation', id: 'conversation', collection: 'conversation', session_id: 'session/raw', replace: true, has_more: false, items: [{ id: 'timeline-entry/raw', sequence: 4561, revision: 3, timestamp: '2026-10-12T11:20:00.123999Z', role: 'assistant', type: 'content', final: true, body: { media_type: 'text/plain', text: '2026-10-12T11:20:00.123999Z agent/unmapped opaque-cursor' } }] } },
  { ordinal: 8, socket_id: null, receive_ms: 30549, kind: 'baseline', value: { last_pre_timeline_ordinal: 7 } },
]
const obj = (value: JsonValue | undefined): JsonObject => {
  if (value !== undefined && value !== null && typeof value === 'object' && !Array.isArray(value)) return value
  throw new Error('expected test object')
}

describe('recording schema-directed N (batch 2)', () => {
  it('rebases against baseline wall time, truncates schema instants, leaves opaque text untouched', () => {
    const normalized = normalizeRecording(script, rows(), anchor)
    expect(normalized[2]?.receive_ms).toBe(0)
    expect(normalized[1]?.receive_ms).toBe(-52)
    const value = normalized[2]?.value
    expect(value?.session_id).toBe('session/proof')
    expect(JSON.stringify(value)).toContain('2026-10-12T11:20:00.123999Z agent/unmapped opaque-cursor')
    expect(JSON.stringify(value)).toContain('"sequence":4561')
    expect(JSON.stringify(value)).toContain('"revision":3')
    expect(normalizeRecording(script, normalized, anchor)).toEqual(normalized)
  })
  it('rebases nullable schema instants exactly once and preserves null', () => {
    const capture = rows()
    const page = obj(obj(obj(capture[1]!.value.body).value).page)
    page.cursor_expires_at = '2026-10-13T03:07:43.034999Z'
    const normalized = normalizeRecording(script, capture, anchor)
    expect(obj(obj(obj(normalized[1]!.value.body).value).page).cursor_expires_at).toBe('2026-01-01T00:00:00.000Z')
    expect(normalizeRecording(script, normalized, anchor)).toEqual(normalized)
    page.cursor_expires_at = null
    expect(obj(obj(obj(normalizeRecording(script, capture, anchor)[1]!.value.body).value).page).cursor_expires_at).toBeNull()
  })
  it('maps Id-suffixed arrangement subjects and folder references and rejects unmapped values', () => {
    const subject = 'arrangement/person/ada/00000000-0000-7000-8000-000000000001'
    const mappedSubject = 'arrangement/person/ada/00000000-0000-7000-8000-000000000002'
    const folder = '00000000-0000-7000-8000-000000000003'
    const mappedFolder = '00000000-0000-7000-8000-000000000004'
    const capture = rows()
    capture.push({ ordinal: 9, socket_id: 'socket#1', receive_ms: 30550, kind: 'command', value: {
      kind: 'subscribe', id: 'arrangements', collection: 'arrangements', person: 'person/ada', subject,
    } })
    capture.push({ ordinal: 10, socket_id: null, receive_ms: 30551, kind: 'attach', value: {
      type: 'arrangement.edit', parameters: { subject, owner: 'person/ada', operations: [{ op: 'folder.create', id: folder, name: 'Proof', parent: folder, key: 'a0' }] },
      api_version: 'st3.client.v0', id: 'agent/example/a', idempotency_key: 'scenario-arrangement-proof',
      fence: { snapshot_id: 'snapshot/meadow/9/opaque', subject_revisions: {} },
    } })
    const mappedScript: RecordingScript = { ...script, id_map: { ...script.id_map, [subject]: mappedSubject, [folder]: mappedFolder } }
    const normalized = normalizeRecording(mappedScript, capture, anchor)
    expect(normalized[4]!.value.subject).toBe(mappedSubject)
    const parameters = obj(normalized[5]!.value.parameters)
    expect(parameters.subject).toBe(mappedSubject)
    const operations = parameters.operations
    if (!Array.isArray(operations)) throw new Error('expected test operations')
    expect(obj(operations[0]).id).toBe(mappedFolder)
    expect(obj(operations[0]).parent).toBe(mappedFolder)
    expect(normalizeRecording(mappedScript, normalized, anchor)).toEqual(normalized)
    expect(() => normalizeRecording(script, capture, anchor)).toThrow(`/4/value/subject: unknown identity ${subject}`)
    const subjectOnly: RecordingScript = { ...script, id_map: { ...script.id_map, [subject]: mappedSubject } }
    expect(() => normalizeRecording(subjectOnly, capture, anchor)).toThrow(`/5/value/parameters/operations/0/id: unknown identity ${folder}`)
  })
  it('maps annotated participant references and rejects unmapped references at their pointers', () => {
    const capture = rows()
    const items = capture[2]!.value.items
    if (!Array.isArray(items)) throw new Error('expected test entries')
    const entry = obj(items[0])
    entry.type = 'message'
    entry.body = { message_id: 'message/raw', from: 'person/example', to: 'step/raw' }
    const mappedScript: RecordingScript = { ...script, id_map: { ...script.id_map, 'message/raw': 'message/proof', 'person/example': 'person/ada', 'step/raw': 'step/proof' } }
    const normalized = normalizeRecording(mappedScript, capture, anchor)
    const outputItems = normalized[2]!.value.items
    if (!Array.isArray(outputItems)) throw new Error('expected test entries')
    expect(obj(outputItems[0]).body).toEqual({ message_id: 'message/proof', from: 'person/ada', to: 'step/proof' })
    expect(normalizeRecording(mappedScript, normalized, anchor)).toEqual(normalized)
    for (const field of ['from', 'to']) {
      const original = obj(entry.body)[field]
      obj(entry.body)[field] = 'person/intruder'
      expect(() => normalizeRecording(mappedScript, capture, anchor)).toThrow(`/2/value/items/0/body/${field}: unknown identity person/intruder`)
      obj(entry.body)[field] = original!
    }
  })
  it('maps annotated actor references including nullable union branches and rejects unmapped actors', () => {
    const capture = rows()
    obj(capture[1]!.value.body).value = {
      id: 'mission/raw', kind: 'mission', revision: 'r1', updated_at: '2026-10-13T03:07:43.034999Z',
      title: 'Proof', state: 'running', mission_revision: 'r1', runs: [], run_generations: {},
      run_details: [{
        id: 'mission-run/raw', requester: 'daemon/raw', status: 'running', phase: 'work',
        progress: { done: 0, total: 1 }, must_act: 'agent', state_since: '2026-10-13T03:07:43.034999Z',
        current_steps: [{ id: 'step-run/raw', title: null, state: 'claimed', since: '2026-10-13T03:07:43.034999Z', assignee: 'person/example', claimant: null }],
      }],
    }
    const mappedScript: RecordingScript = { ...script, id_map: { ...script.id_map, 'mission/raw': 'mission/proof', 'mission-run/raw': 'mission-run/proof', 'step-run/raw': 'step-run/proof', 'daemon/raw': 'daemon/proof', 'person/example': 'person/ada' } }
    const run = (captureRows: readonly RecordingRow[]): JsonObject => {
      const details = obj(obj(captureRows[1]!.value.body).value).run_details
      if (!Array.isArray(details)) throw new Error('expected test run details')
      return obj(details[0])
    }
    const normalized = normalizeRecording(mappedScript, capture, anchor)
    expect(run(normalized).requester).toBe('daemon/proof')
    const steps = run(normalized).current_steps
    if (!Array.isArray(steps)) throw new Error('expected test steps')
    expect(obj(steps[0]).assignee).toBe('person/ada')
    expect(obj(steps[0]).claimant).toBeNull()
    expect(normalizeRecording(mappedScript, normalized, anchor)).toEqual(normalized)
    run(capture).requester = 'daemon/unregistered'
    expect(() => normalizeRecording(mappedScript, capture, anchor)).toThrow('/1/value/body/value/run_details/0/requester: unknown identity daemon/unregistered')
    run(capture).requester = 'daemon/raw'
    const rawSteps = run(capture).current_steps
    if (!Array.isArray(rawSteps)) throw new Error('expected test steps')
    obj(rawSteps[0]).assignee = 'person/intruder'
    expect(() => normalizeRecording(mappedScript, capture, anchor)).toThrow('/1/value/body/value/run_details/0/current_steps/0/assignee: unknown identity person/intruder')
  })
  it('fails unknown identity at the exact schema pointer', () => {
    const capture = rows()
    capture[2]!.value.session_id = 'session/unregistered'
    expect(() => normalizeRecording(script, capture, anchor)).toThrow('/2/value/session_id: unknown identity session/unregistered')
  })
  it('sorts numeric stores rather than receipt order and preserves repeated fences/cursors', () => {
    const capture = rows()
    const first = capture[1]!
    const second: RecordingRow = { ...structuredClone(first), ordinal: 9, receive_ms: 30560 }
    obj(second.value.body).snapshot = snapshot(1)
    const third: RecordingRow = { ...structuredClone(first), ordinal: 10, receive_ms: 30561 }
    capture.push(second, third)
    const normalized = normalizeRecording(script, capture, anchor)
    expect(obj(obj(normalized[1]!.value.body).snapshot).store_index).toBe('store#2')
    expect(obj(obj(normalized[4]!.value.body).snapshot).store_index).toBe('store#1')
    expect(obj(obj(normalized[5]!.value.body).snapshot).store_index).toBe('store#2')
    expect(obj(normalized[1]!.value.body).request_id).toBe(obj(normalized[5]!.value.body).request_id)
    expect(JSON.stringify(normalized)).toContain('cursor#1')
    expect(JSON.stringify(normalized)).not.toContain('cursor#2')
  })
  it('fails a snapshot host mismatch and inconsistent identity fence', () => {
    const capture = rows()
    obj(obj(capture[1]!.value.body).snapshot).host_id = 'host/other'
    expect(() => normalizeRecording(script, capture, anchor)).toThrow('/1/value/body/snapshot/host_id: snapshot host mismatch')
    const valid = rows()
    const extra = structuredClone(valid[1]!)
    obj(extra.value.body).snapshot = snapshot(2, 'snapshot/meadow/9/opaque')
    valid.push({ ...extra, ordinal: 9, receive_ms: 30560 })
    expect(() => normalizeRecording(script, valid, anchor)).toThrow('snapshot equality mismatch')
  })
  it('retains a later action-result fence absent from the HTTP snapshot objects', () => {
    const committed = decodeScript(JSON.parse(readFileSync(new URL('../recordings/fleet-mid-refactor-proof.script.json', import.meta.url), 'utf8')))
    const capture = decodeRecording(readFileSync(new URL('../recordings/fleet-mid-refactor-proof.recording.jsonl', import.meta.url), 'utf8'))
    const action = capture.find((row) => row.kind === 'http' && row.value.method === 'POST')
    if (action === undefined) throw new Error('Missing captured attach exchange')
    obj(obj(action.value.body).value).snapshot_id = 'snapshot/meadow/999999/0123456789abcdef'
    const normalized = normalizeRecording(committed, capture, anchor)
    const result = normalized.find((row) => row.ordinal === action.ordinal)
    if (result === undefined) throw new Error('Missing normalized attach exchange')
    expect(obj(obj(result.value.body).value).snapshot_id).toMatch(/^snapshot\/host\/meadow\/store#[1-9]\d*$/u)
    expect(normalizeRecording(committed, normalized, anchor)).toEqual(normalized)
    obj(obj(action.value.body).value).snapshot_id = 'snapshot/orchard/999999/0123456789abcdef'
    expect(() => normalizeRecording(committed, capture, anchor)).toThrow('snapshot host mismatch')
  })
  it('rejects malformed script/rows and corrupt frame fields', () => {
    expect(() => decodeScript({ format_version: 3 })).toThrow('unsupported version')
    expect(() => decodeRecording('{corrupt')).toThrow('invalid JSON')
    expect(() => decodeRecording(JSON.stringify([{ ordinal: 0, receive_ms: 0, kind: 'frame', socket_id: null, value: {} }]))).toThrow('socket required')
    const capture = rows()
    capture[2]!.value.replace = 'false'
    expect(() => normalizeRecording(script, capture, anchor)).toThrow('malformed schema value')
  })
  it('does not conceal revision, page flag or frame boundary mutations', () => {
    const original = normalizeRecording(script, rows(), anchor)
    const changed = rows()
    changed[2]!.value.has_more = true
    expect(normalizeRecording(script, changed, anchor)).not.toEqual(original)
    const missing = rows().filter((row) => row.ordinal !== 1)
    expect(normalizeRecording(script, missing, anchor)).not.toEqual(original)
    const revised = rows()
    const items = revised[2]!.value.items
    if (!Array.isArray(items)) throw new Error('expected test entries')
    obj(items[0]).revision = 4
    expect(normalizeRecording(script, revised, anchor)).not.toEqual(original)
  })
  it('decodes normalized array artifacts as well as raw NDJSON', () => {
    const capture = rows()
    expect(decodeRecording(JSON.stringify(capture))).toEqual(capture)
    expect(decodeRecording(capture.map((row) => JSON.stringify(row)).join('\n'))).toEqual(capture)
  })
  it('rejects duplicate recording members before parsing JSON arrays and JSONL including escaped-equivalent names', () => {
    for (const key of ['capability', 'stream_capability', 'ordinary']) {
      for (const secondKey of [key, key.replace(/.$/u, (last) => `\\u${last.charCodeAt(0).toString(16).padStart(4, '0')}`)]) {
        const row = `{"ordinal":0,"socket_id":null,"receive_ms":0,"kind":"harness","value":{"nested":{"${key}":"${'live-'}scratch-secret","${secondKey}":"capability#1"}}}`
        expect(() => decodeRecording(`[${row}]`)).toThrow('duplicate JSON member')
        expect(() => decodeRecording(`${row}\n`)).toThrow('duplicate JSON member')
      }
    }
    const valid = '{"ordinal":0,"socket_id":null,"receive_ms":0,"kind":"harness","value":{"items":[{"name":1},{"name":2}],"text":"{ \\"name\\":1, \\"name\\":2 }"}}'
    expect(decodeRecording(valid)).toHaveLength(1)
  })
  it('decodes the committed recording script through the shared strict contract', () => {
    const committed: unknown = JSON.parse(readFileSync(new URL('../recordings/fleet-mid-refactor-proof.script.json', import.meta.url), 'utf8'))
    expect(decodeScript(committed).format_version).toBe(1)
    expect(Object.keys(decodeScript(committed).id_map).length).toBeGreaterThan(0)
  })
  it('rejects unknown script and turn fields from the shared schema property lists', () => {
    expect(() => decodeScript({ ...script, unexpected: true })).toThrow('/unexpected: unexpected field')
    expect(() => decodeScript({ ...script, turns: [{ prompt: 'proof', answer: 'proof', timestamp: '2026-01-01T00:00:00Z', unexpected: true }] })).toThrow('/turns/0/unexpected: unexpected field')
  })
})
