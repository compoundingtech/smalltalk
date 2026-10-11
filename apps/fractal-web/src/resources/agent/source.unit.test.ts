import { ResourceObservation } from '@smalltalk/st3-client/schema'
import { Schema } from 'effect'
import { describe, expect, it } from 'vitest'

import { sortResources } from './model.ts'
import { decodeResourcePage } from './source.ts'

describe('resource collection boundaries', () => {
  it('accepts omitted terminal cursors but refuses a truncated page without a continuation', () => {
    expect(
      decodeResourcePage({
        value: {
          kind: 'page',
          collection: 'resources',
          filters: {},
          items: [],
          page: { has_more: false, limit: 50 },
        },
      }).nextCursor,
    ).toBeNull()
    expect(() =>
      decodeResourcePage({
        value: {
          kind: 'page',
          collection: 'resources',
          filters: {},
          items: [],
          page: { has_more: true, limit: 50 },
        },
      }),
    ).toThrow('invalid paging cursor')
  })
  it('places actionable observations before newer finished items without mutating the retained page', () => {
    const decodeResource = Schema.decodeSync(ResourceObservation)
    const row = (id: ResourceObservation['id'], state: string, observed_at: string) =>
      decodeResource({
        id,
        kind: 'vcs.pull-request',
        facts: { state },
        observed_at,
        opened_by: 'agent/example/worker',
        opened_by_run: null,
      })
    const items = [
      row('resource/merged', 'merged', '2026-10-04T12:00:00Z'),
      row('resource/older-open', 'open', '2026-10-03T10:00:00Z'),
      row('resource/newer-open', 'open', '2026-10-04T10:00:00Z'),
    ]
    expect(sortResources(items).map((item) => item.id)).toEqual([
      'resource/newer-open',
      'resource/older-open',
      'resource/merged',
    ])
    expect(items.map((item) => item.id)).toEqual([
      'resource/merged',
      'resource/older-open',
      'resource/newer-open',
    ])
  })
})
