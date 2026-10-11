import { expect, it, vi } from 'vitest'
import type { CollectionSocket } from '@smalltalk/st3-client'
import { createTestGateway, testSelection, testSnapshot } from './testGateway.ts'
import { selectionParts } from './client.ts'
import { followArrangementInventory, type InventoryEvent } from '../data/arrangements.ts'

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
it('follows the owner-wide window and re-reads the complete list through the generated SDK routes', async () => {
  const gateway = await createTestGateway()
  const sent: unknown[] = []
  const events: InventoryEvent[] = []
  const socket: CollectionSocket = { onopen: null, onmessage: null, onclose: null, onerror: null, close: () => undefined, send: (body) => sent.push(JSON.parse(body)) }
  const follow = followArrangementInventory({ gateway: gateway.sdk, owner: testSelection.owner, socket: () => socket, onEvent: (event) => events.push(event) })
  try {
    await vi.waitFor(() => expect(socket.onopen).not.toBeNull())
    socket.onopen?.()
    await vi.waitFor(() => expect(sent).toEqual([{ kind: 'subscribe', id: 'arrangements-inventory', collection: 'arrangements', person: testSelection.owner, limit: 1 }]))
    const send = (frame: unknown): void => socket.onmessage?.({ data: JSON.stringify(frame) })
    send({ kind: 'snapshot', id: 'arrangements-inventory', collection: 'arrangements', has_more: true, items: [], order: [], snapshot: testSnapshot })
    await vi.waitFor(() => expect(events).toHaveLength(1))
    gateway.state.current.body.name.value = 'Renamed outside the window'
    send({ kind: 'changes', id: 'arrangements-inventory', collection: 'arrangements', has_more: true, upserts: [], removes: [], order: [], snapshot: testSnapshot })
    await vi.waitFor(() => expect(events).toHaveLength(2))
    expect(events.map((event) => event._tag === 'Complete' ? event.inventory.items.map((item) => item.body.name.value) : event._tag))
      .toEqual([['Sidebar'], ['Renamed outside the window']])
    expect(gateway.calls.filter((call) => call.url.startsWith('/v1/client/arrangements?')).map((call) => call.url))
      .toEqual(['/v1/client/arrangements?person=person%2Fexample&limit=100', '/v1/client/arrangements?person=person%2Fexample&limit=100'])
  } finally { follow.close(); await gateway.close() }
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
it('ends malformed owner-wide frames once without reading or publishing their rows', async () => {
  const gateway = await createTestGateway()
  let closed = 0
  const events: InventoryEvent[] = []
  const socket: CollectionSocket = { onopen: null, onmessage: null, onclose: null, onerror: null, close: () => { closed++ }, send: () => undefined }
  const follow = followArrangementInventory({ gateway: gateway.sdk, owner: testSelection.owner, socket: () => socket, onEvent: (event) => events.push(event) })
  try {
    await vi.waitFor(() => expect(socket.onmessage).not.toBeNull())
    const onmessage = socket.onmessage
    onmessage?.({ data: JSON.stringify({ kind: 'changes', id: 7, collection: 'arrangements' }) })
    onmessage?.({ data: JSON.stringify({ kind: 'changes', id: 'arrangements-inventory', collection: 'arrangements' }) })
    await vi.waitFor(() => expect(closed).toBe(1))
    expect(events.map((event) => event._tag)).toEqual(['Interrupted'])
    expect(gateway.calls.some((call) => call.url.startsWith('/v1/client/arrangements'))).toBe(false)
  } finally { follow.close(); await gateway.close() }
})
