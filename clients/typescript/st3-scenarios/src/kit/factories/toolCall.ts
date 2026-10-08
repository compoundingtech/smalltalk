import type { TimelineEntry } from '@smalltalk/st3-client'

import type { FactoryContext } from '../context.ts'
import { entry, type Thread } from './turn.ts'

export interface ToolCallInput {
  readonly atMs: number
  readonly name: string
  readonly arguments: Record<string, unknown>
  /** `pending` emits the call without a result. */
  readonly outcome: 'ok' | 'error' | 'pending'
  readonly output?: string
  readonly durationMs?: number
}

/** A `tool_call` entry and, unless pending, its `tool_result` joined by call id. */
export const toolCall = (ctx: FactoryContext, cursor: Thread, input: ToolCallInput): TimelineEntry[] => {
  const call = entry(ctx, cursor, {
    atMs: input.atMs,
    role: 'assistant',
    type: 'tool_call',
    body: { call_id: `call-${cursor.agent.role}-${cursor.sequence + 1}`, name: input.name, arguments: input.arguments },
  })
  if (input.outcome === 'pending') return [call]
  const result = entry(ctx, cursor, {
    atMs: input.atMs + (input.durationMs ?? 1_500),
    role: 'tool',
    type: 'tool_result',
    body: {
      call_id: (call.body as { call_id: string }).call_id,
      status: input.outcome === 'ok' ? 'success' : 'error',
      media_type: 'text/plain',
      content: input.output ?? '',
    },
  })
  return [call, result]
}
