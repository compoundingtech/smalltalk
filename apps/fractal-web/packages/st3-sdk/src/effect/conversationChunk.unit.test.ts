import { createRequire } from 'node:module'
import type { CollectionFrame } from '@smalltalk/st3-client'
import { decodeUnknownSync, TimelineEntry } from '@smalltalk/st3-client/schema'
import * as Schema from 'effect/Schema'
import { describe, expect, it } from 'vitest'

import { decodeConversationChunk } from './mod.ts'

const fixtures = Schema.decodeUnknownSync(Schema.Struct({ items: Schema.Array(Schema.Unknown) }))(
  createRequire(import.meta.url)('../../../../../../docs/st3/client-v0/fixtures/timeline-conformance.json'),
).items

const frame: Extract<CollectionFrame, { kind: 'conversation' }> = {
  kind: 'conversation', collection: 'conversation', id: 'follow/1',
  session_id: 'session/1', replace: true, items: [],
}

describe('native conversation empty evidence', () => {
  it('requires an explicitly complete native replace page', () => {
    expect(decodeConversationChunk({ ...frame, has_more: false }).observation).toEqual({ empty: true })
    expect(decodeConversationChunk({ ...frame, has_more: true }).observation).toEqual({ empty: false })
    expect(decodeConversationChunk(frame).observation).toEqual({ empty: false })
  })

  it('does not infer native emptiness from a payload rejected by the entry schema', () => {
    const decoded = decodeConversationChunk({ ...frame, has_more: false, items: [{
      id: 'entry/1', sequence: 1, revision: 1, final: true, role: 'assistant',
      timestamp: 'not-a-timestamp', type: 'content', body: { media_type: 'text/plain', text: 'Native row' },
    }] })
    expect(decoded.entries[0]?.type).toBe('unrecognized')
    expect(decoded.observation).toEqual({ empty: false })
  })

  it('an empty delta is not a native empty-page observation', () => {
    expect(decodeConversationChunk({ ...frame, replace: false, has_more: false }).observation).toBeUndefined()
  })

  it('keeps producer-conformant tool, status, job and truncation entries typed', () => {
    const decodeEntry = decodeUnknownSync(TimelineEntry, 'strict')
    expect(fixtures.length).toBeGreaterThan(0)
    for (const entry of fixtures) {
      expect(() => decodeEntry(entry)).not.toThrow()
    }
  })

  it('explicitly records the known daemon sender schema gap (issue #1957)', () => {
    // https://github.com/compoundingtech/smalltalk/issues/1957 — pending sender amendment.
    const decoded = decodeConversationChunk({ ...frame, items: [{
      id: 'timeline-entry/ada', sequence: 0, revision: 1, final: true,
      timestamp: '2026-10-12T00:00:00.000Z', role: 'system', type: 'message',
      body: { message_id: 'message/tester', from: 'daemon/runtime', to: 'agent/ada' },
    }] })
    expect(decoded.entries[0]?.type).toBe('unrecognized')
  })
})
