import { describe, expect, it } from 'vitest'
import type { ToolCallItem } from '../conversation/model.ts'
import { capturedChanges, capturedProvenance, selectedCapturedChange } from './capturedChanges.ts'

const call: ToolCallItem = {
  _tag: 'ToolCall', id: 'observed-call', callId: 'call/edit', name: 'Edit',
  input: { file_path: 'src/example.ts', old_string: 'old', new_string: 'new' },
  status: 'success', result: { content: 'Edit applied', isError: false, at: '2026-10-07T00:00:01Z' },
  callSeen: true, at: '2026-10-07T00:00:00Z',
}
const changes = (items: readonly ToolCallItem[], hasOlder = false) => capturedChanges({ items, hasOlder })

describe('public conversation change captures', () => {
  it('requires successful calls and nonerror observed results', () => {
    for (const status of ['running', 'error', 'interrupted'] as const) expect(changes([{ ...call, status }])).toEqual([])
    expect(changes([{ ...call, result: undefined }])).toEqual([])
    expect(changes([{ ...call, result: { content: 'refused', isError: true, at: '2026-10-07T00:00:01Z' } }])).toEqual([])
    expect(changes([call])).toHaveLength(1)
  })
  it('shows exact excerpt counts and content, never worktree totals', () => {
    const [value] = changes([call], true)
    expect(value?.diff).toMatchObject({ path: 'src/example.ts', added: 1, removed: 1, excerpt: true })
    expect(value?.resource.fileCount).toEqual({ _tag: 'Known', value: 1 })
    expect(value?.resource.files).toEqual([{ path: 'src/example.ts', added: { _tag: 'Known', value: 1 }, removed: { _tag: 'Known', value: 1 }, lines: { _tag: 'Known', value: ['-old', '+new'] } }])
    expect(value?.resource.detail).toContain('Captured successful-tool edited excerpt')
    expect(value?.resource.detail).toContain('Older transcript history is outside this loaded window')
  })
  it('preserves recorded unified patch scope including orphan results', () => {
    const patch = '--- a/src/example.ts\n+++ b/src/example.ts\n@@ -1,1 +1,2 @@\n old\n+new'
    const [value] = changes([{ ...call, input: undefined, callSeen: false, result: { content: patch, isError: false, at: '2026-10-07T00:00:01Z' } }])
    expect(value?.diff).toMatchObject({ excerpt: false, added: 1, removed: 0 })
    expect(value?.resource.detail).toContain('Captured successful-tool patch')
    expect(value?.resource.detail).toContain('loaded transcript window')
  })
  it('does not invent unknown paths, content, or unchanged changes', () => {
    expect(changes([{ ...call, input: { old_string: 'old', new_string: 'new' } }])).toEqual([])
    expect(changes([{ ...call, input: { file_path: 'src/example.ts', content: 'opaque' } }])).toEqual([])
    expect(changes([{ ...call, input: { file_path: 'src/example.ts', old_string: 'same', new_string: 'same' } }])).toEqual([])
  })
  it('preserves reported senders without attributing absent senders to the selected agent', () => {
    const first = { ...call, sender: { kind: 'subagent' as const, label: 'worker-a', via: 'delegation' } }
    const second = { ...call, id: 'second', sender: { kind: 'human' as const, label: 'person-b' } }
    const snapshot = changes([first, second, { ...call, id: 'unknown' }])
    expect(snapshot.map(value => value.diff.path)).toEqual(['src/example.ts', 'src/example.ts', 'src/example.ts'])
    expect(snapshot[0]?.provenance).toEqual({ sender: first.sender, name: call.name, callId: call.callId, at: call.at })
    expect(snapshot[1]?.provenance.sender).toEqual(second.sender)
    expect(capturedProvenance(snapshot[0]!)).toContain('Reported sender: worker-a (subagent) via delegation')
    expect(capturedProvenance(snapshot[1]!)).toContain('Reported sender: person-b (human)')
    expect(snapshot[2]?.provenance.sender).toBeUndefined()
    expect(capturedProvenance(snapshot[2]!)).toContain('Reported sender unknown · Tool: Edit · Call: call/edit')
  })
  it('keeps repeated captures distinct and local selection inside the authoritative snapshot', () => {
    const snapshot = changes([call, { ...call, id: 'second-call', input: { file_path: 'src/example.ts', old_string: 'new', new_string: 'final' } }])
    expect(snapshot).toHaveLength(2)
    expect(snapshot[0]?.id).not.toBe(snapshot[1]?.id)
    expect(selectedCapturedChange({ changes: snapshot, id: undefined })).toBe(snapshot[1])
    expect(selectedCapturedChange({ changes: snapshot, id: snapshot[0]?.id })).toBe(snapshot[0])
    expect(selectedCapturedChange({ changes: snapshot.slice(1), id: snapshot[0]?.id })).toBe(snapshot[1])
    expect(selectedCapturedChange({ changes: [], id: snapshot[0]?.id })).toBeUndefined()
    expect(call.input).toEqual({ file_path: 'src/example.ts', old_string: 'old', new_string: 'new' })
  })
})
