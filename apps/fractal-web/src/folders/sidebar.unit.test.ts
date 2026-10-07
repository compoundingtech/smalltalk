import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { expect, it, vi } from 'vitest'
import { arrangementSidebarDoc, folders } from './client.ts'
import { testArrangement, testCapabilities, testSnapshot } from './testGateway.ts'
import { project } from './core.mts'

it('fails unavailable before listing or editing when arrangements are ungranted', async () => {
  const requests: string[] = []
  vi.stubGlobal('location', { origin: 'http://sidebar.test' })
  vi.stubGlobal('fetch', async (input: RequestInfo | URL) => {
    requests.push(String(input))
    return Response.json({ api_version: 'st3.client.v0', request_id: 'request/test', snapshot: testSnapshot,
      value: { ...testCapabilities, capabilities: [{ id: 'arrangements', state: 'ungranted', version: 1 }] } })
  })
  const registry = AtomRegistry.make()
  const unsubscribe = registry.subscribe(folders, () => {})
  try {
    await vi.waitFor(() => expect(registry.get(folders).phase).toBe('unavailable'))
    expect(registry.get(folders).detail).toContain('Granted arrangements capability version 1')
    expect(registry.get(folders).readOnly).toBe(true)
    expect(requests).toEqual(['http://sidebar.test/v1/client/capabilities'])
    expect(project(registry.get(folders).doc, ['agent/a']).unfiled).toEqual(['agent/a'])
    expect(() => registry.get(folders).edit([])).toThrow('read-only')
  } finally { unsubscribe(); registry.dispose(); vi.unstubAllGlobals() }
})

it('projects authoritative register values and tombstones without editing an arrangement', () => {
  const arrangement = testArrangement()
  const id = '00000000-0000-7000-8000-000000000002'
  arrangement.body.folders[id] = {
    name: { value: 'Inbox', revision: 'claim/name' },
    position: { value: { parent: null, key: 'a0' }, revision: 'claim/position' },
    tombstone: null,
  }
  arrangement.body.placements['agent/a'] = { value: { folder: id, key: 'a0' }, revision: 'claim/place' }
  const original = JSON.stringify(arrangement)
  expect(project(arrangementSidebarDoc(arrangement), ['agent/a']).folders[0]).toMatchObject({ id, name: 'Inbox', members: ['agent/a'] })
  expect(JSON.stringify(arrangement)).toBe(original)
})
