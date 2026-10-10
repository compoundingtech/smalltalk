import { createServer } from 'node:http'
import { once } from 'node:events'
import { St3Client } from '@smalltalk/st3-client'
import type { Arrangement, Capabilities, Snapshot } from '@smalltalk/st3-client'
import { createFoldersClient } from './client.ts'
import type { FoldersClient, Selection } from './client.ts'

export const testSelection: Selection = { owner: 'person/example', subject: 'arrangement/person/example/00000000-0000-7000-8000-000000000001' }
export const testSnapshot: Snapshot = { id: 'snapshot/example/1/proof', host_id: 'host/example', store_index: 1, projection_version: 'client-projection.v0', created_at: '2026-01-01T00:00:00Z' }
export const testCapabilities: Capabilities = { kind: 'capabilities', capabilities: [{ id: 'arrangements', state: 'granted', version: 1 }], event_cursor: 'cursor/1', oldest_event_cursor: 'cursor/1', schemas: ['https://example.test/client-v0.schema.json'], session_actor: 'person/example', transport: 'fabric-loopback', limits: { max_event_items: 200, max_page_items: 200, max_wait_ms: 30000, max_response_bytes: 1048576 } }
export const testArrangement = (): Arrangement => ({ ...testSelection, id: testSelection.subject, kind: 'arrangement', revision: 'claim/import', updated_at: '2026-01-01T00:00:00Z', deleted: false, body: { version: 1, name: { value: 'Sidebar', revision: 'claim/import' }, folders: {}, placements: {} } })
export interface TestGateway {
  readonly client: FoldersClient
  readonly sdk: St3Client
  readonly calls: { url: string; body?: unknown }[]
  readonly state: { current: Arrangement; accepted: boolean; loseNextReply: boolean; capability: boolean; afterAccept?: () => void }
  close(): Promise<void>
}
/** Local HTTP implementation of the exercised wire fixture, not a live daemon proof. */
export const createTestGateway = async (): Promise<TestGateway> => {
  const calls: { url: string; body?: unknown }[] = []
  const state: TestGateway['state'] = { current: testArrangement(), accepted: false, loseNextReply: false, capability: true }
  const server = createServer((request, response) => { void (async () => {
    const url = request.url ?? ''
    let body: unknown
    if (request.method === 'POST') {
      const chunks: Buffer[] = []
      for await (const chunk of request) chunks.push(Buffer.from(chunk))
      body = JSON.parse(Buffer.concat(chunks).toString('utf8'))
    }
    calls.push({ url, ...(body === undefined ? {} : { body }) })
    const send = (value: unknown): void => { response.writeHead(200, { 'content-type': 'application/json' }); response.end(JSON.stringify({ api_version: 'st3.client.v0', request_id: 'request/test', snapshot: testSnapshot, value })) }
    if (url === '/v1/client/capabilities') { send({ ...testCapabilities, capabilities: state.capability ? testCapabilities.capabilities : [] }); return }
    if (url === '/v1/client/actions') {
      if (!state.accepted) { state.accepted = true; state.afterAccept?.() }
      if (state.loseNextReply) { state.loseNextReply = false; response.destroy(); return }
      send({ kind: 'action-result', action_id: 'action/import', operation_id: 'operation/import', snapshot_id: testSnapshot.id, status: 'completed', affected_ids: [testSelection.subject], arrangement_revision: 'claim/import' }); return
    }
    if (url.startsWith('/v1/client/arrangements?')) { send({ kind: 'page', collection: 'arrangements', filters: { person: testSelection.owner }, items: [state.current], page: { limit: 100, has_more: false, next_cursor: null, cursor_expires_at: null } }); return }
    if (url.startsWith('/v1/client/arrangements/')) { send(state.current); return }
    response.writeHead(404); response.end()
  })().catch(() => response.destroy()) })
  server.listen(0, '127.0.0.1')
  await once(server, 'listening')
  const address = server.address()
  if (address === null || typeof address === 'string') throw new TypeError('Missing fixture TCP listener')
  const sdk = new St3Client({ baseUrl: `http://127.0.0.1:${address.port}` })
  return { sdk, client: createFoldersClient(sdk, testSelection), calls, state, close: async () => {
    server.closeAllConnections()
    await new Promise<void>((resolve) => server.close(() => resolve()))
  } }
}
