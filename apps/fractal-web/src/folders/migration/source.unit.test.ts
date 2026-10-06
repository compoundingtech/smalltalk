import { expect, it } from 'vitest'
import { createLegacyClaimsSource, foldLegacyClaims } from './source.ts'

const claim = { kind: 'custom.fractal.sidebar', actor: 'person/example', body: { fields: { v: 1, folders: { '00000000-0000-7000-8000-000000000001': { name: { value: 'Inbox', at: [1, 0, 'writer'] }, position: { parent: null, key: 'V', at: [1, 0, 'writer'] } } }, placements: {} } } }
it('continues after unchanged/unrelated pages and retains high-water indexes', async () => {
  const calls: (string | undefined)[] = []
  const result = await foldLegacyClaims(async (cursor) => {
    calls.push(cursor)
    return cursor === undefined ? { claims: [], nextCursor: '100', storeIndex: 100 } : { claims: [claim], storeIndex: 1 }
  })
  expect(calls).toEqual([undefined, '100'])
  expect(Object.values(result.doc.folders)[0]?.name?.value).toBe('Inbox')
  expect(result.storeIndex).toBe(100)
})
it('refuses cyclic pagination', async () => {
  await expect(foldLegacyClaims(async () => ({ claims: [], nextCursor: '100', storeIndex: 100 }))).rejects.toThrow('did not advance')
})
it('does not read any persistence unless the source fence is held', async () => {
  let reads = 0
  const source = createLegacyClaimsSource({ page: async () => { reads++; return { claims: [], storeIndex: 0 } }, fence: {
    freeze: async () => ({ source: 'source/example', token: 'fence/example' }), assertFrozen: async () => { throw new TypeError('writer still active') },
  } })
  await expect(source.readFrozen({ source: 'source/example', token: 'fence/example' })).rejects.toThrow('writer still active')
  expect(reads).toBe(0)
})
it('detects fence revocation after enumeration', async () => {
  let assertions = 0
  const source = createLegacyClaimsSource({ page: async () => ({ claims: [claim], storeIndex: 1 }), fence: {
    freeze: async () => ({ source: 'source/example', token: 'fence/example' }), assertFrozen: async () => { if (++assertions > 1) throw new TypeError('fence revoked') },
  } })
  await expect(source.readFrozen({ source: 'source/example', token: 'fence/example' })).rejects.toThrow('fence revoked')
})
