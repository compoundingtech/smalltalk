import { describe, expect, it } from 'vitest'
import { decodeConversationEntry } from './conversationBlocks.ts'
import { decodeConversationChunk } from './mod.ts'

const rawEntry = (blocks: readonly unknown[]) => ({ id: 'timeline-entry/per-block/1', sequence: 1, revision: 1, final: true, role: 'tool', timestamp: '2026-10-12T12:00:00Z', type: 'tool_result', body: { call_id: 'call/a', content: 'output', media_type: 'text/plain', status: 'success', blocks } })
const block = { id: 'block/a', kind: 'tool_output', source_type: 'omp', payload: {} }

describe('per-block conversation decoding', () => {
  it('keeps a float wall_ms bash result without its invalid view', () => {
    const decoded = decodeConversationEntry(rawEntry([{ ...block, view: { type: 'bash', tool: 'bash', env_keys: [], wall_ms: 1.25 } }]))
    expect(decoded).toMatchObject({ _tag: 'Entry', entry: { type: 'tool_result', body: { blocks: [block] }, degraded: [{ _tag: 'ViewDropped', kind: 'tool_output', viewType: 'bash' }] } })
    if (decoded._tag === 'Entry' && decoded.entry.type === 'tool_result') expect(decoded.entry.body.blocks?.[0]).not.toHaveProperty('view')
  })
  it('drops a bad block, keeps its raw, and keeps the good block', () => {
    const bad = { ...block, kind: 3 }
    expect(decodeConversationEntry(rawEntry([bad, block]))).toMatchObject({ _tag: 'Entry', entry: { body: { blocks: [block] }, degraded: [{ _tag: 'BlockDropped', raw: bad }] } })
  })
  it('drops future views according to st fallback contract', () => {
    expect(decodeConversationEntry(rawEntry([{ ...block, view: { type: 'future-view', extra: 3 } }]))).toMatchObject({ _tag: 'Entry', entry: { body: { blocks: [block] }, degraded: [{ _tag: 'ViewDropped', viewType: 'future-view' }] } })
  })
  it('keeps undecodable entries as unrecognized with a decode issue', () => {
    const raw = { ...rawEntry([]), timestamp: 'bad' }
    const chunk = decodeConversationChunk({ kind: 'conversation', collection: 'conversation', id: 'follow/a', session_id: 'session/a', replace: true, items: [raw] })
    expect(chunk.entries[0]).toMatchObject({ type: 'unrecognized', raw })
    expect(chunk.entries[0]?.type === 'unrecognized' ? chunk.entries[0].issue : '').not.toBe('')
  })
})
