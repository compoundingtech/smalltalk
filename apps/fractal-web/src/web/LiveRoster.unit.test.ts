import type { Undecodable } from '@st3/sdk/effect'
import { expect, test, vi } from 'vitest'

vi.mock('@stylexjs/stylex', () => import('../testStylex.ts'))

import { hiddenRoster, rosterDecodeNotice } from './LiveRoster.tsx'

const failure = (fields: Omit<Undecodable, '_tag'>): Undecodable => ({ _tag: 'Rejected', ...fields })
const omittedLeaf: Undecodable = { _tag: 'Degraded', id: 'agent/kept', path: 'usage.aggregation', reason: 'Unrecognized leaf value' }
const brokenFields = [
  failure({ id: 'agent/broken', path: 'runtime_ids', reason: 'Expected array' }),
  failure({ id: 'agent/broken', path: 'updated_at', reason: 'Expected timestamp' }),
]
const brokenIdentity = failure({ path: 'id', reason: 'Expected string' })

test('a partial roster counts each hidden row once and never a shown row with an omitted leaf', () => {
  const hidden = hiddenRoster({
    decode: { undecodable: [omittedLeaf, ...brokenFields, brokenIdentity], decodeStale: false },
    shown: new Set(['agent/kept']),
  })
  expect(hidden.count).toBe(2)
  expect(hidden.failures).toEqual([...brokenFields, brokenIdentity])
})

test('an all-bad frame counts every failed row, including ids still shown from the last-good roster', () => {
  const hidden = hiddenRoster({
    decode: { undecodable: [omittedLeaf, ...brokenFields, brokenIdentity], decodeStale: true },
    shown: new Set(['agent/kept', 'agent/broken']),
  })
  expect(hidden.count).toBe(2)
  expect(hidden.failures).toHaveLength(3)
})

test('a fully decoded roster has no notice', () => {
  expect(hiddenRoster({ decode: { undecodable: [], decodeStale: false }, shown: new Set() }).count).toBe(0)
  expect(hiddenRoster({ decode: { undecodable: [omittedLeaf], decodeStale: false }, shown: new Set(['agent/kept']) }).count).toBe(0)
})

test('the notice wording is the contract sentence', () => {
  expect(rosterDecodeNotice(1)).toBe('1 agent could not be shown · this view is older than the daemon')
  expect(rosterDecodeNotice(2)).toBe('2 agents could not be shown · this view is older than the daemon')
})
