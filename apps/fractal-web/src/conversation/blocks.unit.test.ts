import { describe, expect, it } from 'vitest'
import { decodeConversationChunk } from '@st3/sdk/effect'
import { LiveTimeline } from './fromTimeline.ts'
import { nativeTurnFacts } from './assistantFacts.ts'

const entry = (type: string, sequence: number, body: unknown) => ({ id: `timeline-entry/example/${sequence}`, sequence, revision: 1, final: true, role: type === 'content' ? 'assistant' : 'tool', timestamp: '2026-01-15T12:00:00Z', type, body })
const block = (kind: string, fields: object) => ({ id: `block/${kind}`, kind, source_type: 'omp', payload: { body_ref: true }, ...fields })
const frame = (items: readonly unknown[], header?: unknown) => decodeConversationChunk({ kind: 'conversation', collection: 'conversation', id: 'follow/example', session_id: 'session/example', replace: true, has_more: false, items, header })

describe('optional native block facts', () => {
  it('reads deployed result metadata and retains negotiated views through SDK decoding', () => {
    const timeline = new LiveTimeline()
    timeline.apply(frame([
      entry('tool_call', 1, { call_id: 'call/example', name: 'bash', arguments: { command: 'printf ready' }, blocks: [block('tool_call', { view: { type: 'bash', tool: 'bash', command: 'printf ready', env_keys: [] } })] }),
      entry('tool_result', 2, { call_id: 'call/example', content: 'ready', media_type: 'text/plain', status: 'success', blocks: [block('tool_output', { metadata: { exitCode: 0, wallTimeMs: 0, future: { unit: 'ticks', value: 4 } }, view: { type: 'bash', tool: 'bash', env_keys: [], exit_code: 0, wall_ms: 0 } })] }),
    ]))
    const call = timeline.project().items[0]
    expect(call).toMatchObject({ _tag: 'ToolCall', view: { type: 'bash' }, result: { metadata: { exitCode: 0, wallTimeMs: 0, future: { unit: 'ticks', value: 4 } }, view: { exit_code: 0, wall_ms: 0 } } })
  })
  it('keeps old fallback-only pages readable without inventing facts', () => {
    const timeline = new LiveTimeline()
    timeline.apply(frame([entry('content', 1, { text: 'Ready.', media_type: 'text/plain' })]))
    expect(timeline.project().items[0]).toMatchObject({ _tag: 'Text', text: 'Ready.' })
    expect(timeline.project().items[0]).not.toHaveProperty('assistantMetadata')
    expect(timeline.header).toBeUndefined()
  })
  it('retains annotated headers and exposes response metadata once, including zero cost', () => {
    const timeline = new LiveTimeline()
    const header = { title: { value: 'Example work', source: 'harness', as_of: '2026-01-15T12:00:00Z' } }
    timeline.apply(frame([entry('content', 1, { text: 'Ready.', media_type: 'text/plain', blocks: [block('text', { metadata: { model: 'example-model', usage: { total: 42, cost_usd: 0 }, duration_ms: 12 } })] })], header))
    expect(timeline.header).toEqual(header)
    expect(nativeTurnFacts({ items: timeline.project().items })).toEqual({ model: 'example-model', tokens: 42, costUsd: 0 })
    timeline.apply({ entries: [], replace: false, hasMore: false })
    expect(timeline.header).toEqual(header)
    timeline.apply({ entries: [], replace: true, hasMore: false })
    expect(timeline.header).toBeUndefined()
  })
})
