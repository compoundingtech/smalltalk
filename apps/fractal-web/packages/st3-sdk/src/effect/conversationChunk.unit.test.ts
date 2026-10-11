import type { CollectionFrame } from '@smalltalk/st3-client'
import { describe, expect, it } from 'vitest'

import { decodeConversationChunk } from './mod.ts'

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
      timestamp: 'not-a-timestamp', type: 'content', body: { text: 'Native row' },
    }] })
    expect(decoded.entries[0]?.type).toBe('unrecognized')
    expect(decoded.observation).toEqual({ empty: false })
  })

  it('an empty delta is not a native empty-page observation', () => {
    expect(decodeConversationChunk({ ...frame, replace: false, has_more: false }).observation).toBeUndefined()
  })
})
