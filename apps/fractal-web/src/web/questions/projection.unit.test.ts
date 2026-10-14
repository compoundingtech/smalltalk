import { describe, expect, it } from 'vitest'
import type { ConversationItem } from '../../conversation/model.ts'
import { pendingQuestionRequests } from './projection.ts'

const ask: ConversationItem = { _tag: 'ToolCall', id: 'entry/ask', callId: 'call/ask', name: 'ask', callSeen: true, status: 'running', at: '2026-10-01T12:00:00Z', input: { questions: [{ id: 'checks', question: 'Which checks should run?', multi: true, options: [{ label: 'Typecheck', description: 'Check the changed packages' }] }] } }
describe('native pending question projection', () => {
  it('uses kit question fields and never invents a web settlement capability', () => {
    expect(pendingQuestionRequests([ask], 'Sample agent')).toMatchObject([{ id: 'call/ask', answerable: false, state: { _tag: 'pending' }, questions: [{ id: 'checks', prompt: 'Which checks should run?', kind: 'multi', options: [{ id: 'o0', label: 'Typecheck' }] }] }])
  })
  it('does not promote completed, interrupted, malformed or unrelated tools', () => {
    for (const change of [{ status: 'success' as const }, { status: 'interrupted' as const }, { input: { questions: [{ question: 1 }] } }, { name: 'read' }]) {
      expect(pendingQuestionRequests([{ ...ask, ...change }], 'Sample agent')).toEqual([])
    }
  })
  it('does not create a stale pending card after a native answer arrives', () => {
    expect(pendingQuestionRequests([{ ...ask, result: { content: { selectedOptions: ['Typecheck'] }, at: ask.at, isError: false } }], 'Sample agent')).toEqual([])
  })
  it('renders an ask without options as a read-only text question', () => {
    expect(pendingQuestionRequests([{ ...ask, input: { questions: [{ question: 'Where should this deploy?' }] } }], 'Sample agent')[0]?.questions[0]).toMatchObject({ kind: 'text', options: [] })
  })
})
