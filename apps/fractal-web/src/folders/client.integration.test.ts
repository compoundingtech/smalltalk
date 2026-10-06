import { expect, it } from 'vitest'
import type { CollectionSocket } from '@smalltalk/st3-client'
import { createTestGateway, testArrangement, testSelection, testSnapshot } from './testGateway.ts'
import { selectionParts } from './client.ts'

it('uses real generated SDK arrangements routes, explicit owner and action request shape', async () => {
  const gateway = await createTestGateway()
  try {
    await gateway.client.read()
    await gateway.client.list('cursor/example')
    await gateway.client.edit({ id: 'action/test', idempotency_key: 'edit/test', fence: { snapshot_id: testSnapshot.id, subject_revisions: {} } }, [{ op: 'folder.rename', id: '00000000-0000-7000-8000-000000000002', name: 'Renamed' }])
    expect(gateway.calls.map((call) => call.url)).toContain('/v1/client/arrangements/example/00000000-0000-7000-8000-000000000001')
    expect(gateway.calls.map((call) => call.url)).toContain('/v1/client/arrangements?person=person%2Fexample&cursor=cursor%2Fexample')
    expect(gateway.calls.find((call) => call.url === '/v1/client/actions')?.body).toMatchObject({ api_version: 'st3.client.v0', type: 'arrangement.edit', parameters: { owner: testSelection.owner, subject: testSelection.subject } })
    expect(gateway.calls.every((call) => call.url.startsWith('/v1/client/'))).toBe(true)
  } finally { await gateway.close() }
})
it('refuses missing arrangements grants before any arrangement read or mutation', async () => {
  const gateway = await createTestGateway()
  gateway.state.capability = false
  try {
    await expect(gateway.client.read()).rejects.toThrow('capability version 1')
    expect(gateway.calls).toHaveLength(1)
  } finally { await gateway.close() }
})
it('refuses a mismatched owner or non-UUIDv7 subject', () => {
  expect(() => selectionParts({ ...testSelection, owner: 'person/other' })).toThrow()
  expect(() => selectionParts({ ...testSelection, subject: 'arrangement/person/example/not-a-uuid' })).toThrow()
})
it('subscribes to the selected arrangement and applies authoritative snapshot, changes and retirement', async () => {
  const gateway = await createTestGateway()
  const sent: unknown[] = []
  const values: (string | undefined)[] = []
  const socket: CollectionSocket = { onopen: null, onmessage: null, onclose: null, onerror: null, close: () => undefined, send: (body) => sent.push(JSON.parse(body)) }
  try {
    const stream = await gateway.client.watch((value) => values.push(value?.body.name.value), { socket: () => socket })
    socket.onopen?.()
    expect(sent).toContainEqual({ kind: 'subscribe', id: 'folders', collection: 'arrangements', person: testSelection.owner, limit: 100, subject: testSelection.subject })
    const send = (frame: unknown): void => socket.onmessage?.({ data: JSON.stringify(frame) })
    send({ kind: 'snapshot', id: 'folders', collection: 'arrangements', has_more: false, items: [testArrangement()], order: [testSelection.subject], snapshot: testSnapshot })
    const renamed = testArrangement()
    renamed.body.name.value = 'Concurrent rename'
    send({ kind: 'changes', id: 'folders', collection: 'arrangements', has_more: false, upserts: [renamed], removes: [], order: [testSelection.subject], snapshot: testSnapshot })
    send({ kind: 'changes', id: 'folders', collection: 'arrangements', has_more: false, upserts: [], removes: [testSelection.subject], order: [], snapshot: testSnapshot })
    expect(values).toEqual(['Sidebar', 'Concurrent rename', undefined])
    stream.close()
  } finally { await gateway.close() }
})
it('validates generated resource shapes rather than trusting compile-time SDK types', async () => {
  const gateway = await createTestGateway()
  try {
    gateway.state.current.body.name.value = ''
    await expect(gateway.client.read()).rejects.toThrow('Arrangement response')
    gateway.state.current.body.name.value = 'Sidebar'
    gateway.state.current.owner = 'person/other'
    await expect(gateway.client.read()).rejects.toThrow('Arrangement response')
  } finally { await gateway.close() }
})
it('terminates malformed selected collection frames once without publishing invalid rows', async () => {
  const gateway = await createTestGateway()
  let closed = 0
  let errors = 0
  let values = 0
  const socket: CollectionSocket = { onopen: null, onmessage: null, onclose: null, onerror: null, close: () => { closed++ }, send: () => undefined }
  try {
    await gateway.client.watch(() => { values++ }, { socket: () => socket, onEnd: () => { errors++ } })
    socket.onmessage?.({ data: JSON.stringify({ kind: 'changes', id: 'folders', collection: 'arrangements' }) })
    socket.onmessage?.({ data: '{}' })
    expect(closed).toBe(1)
    expect(errors).toBe(1)
    expect(values).toBe(0)
  } finally { await gateway.close() }
})
