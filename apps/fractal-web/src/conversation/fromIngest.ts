// Map `@overeng/agent-session-ingest` `NormalizedRecord`s (offline harness history) to
// `ConversationItem`s.
//
// `IngestRecord` mirrors effect-utils `packages/@overeng/agent-session-ingest/src/normalized/
// schema.ts` in its *encoded* form (timestamps as ISO strings). It is copied, not imported: the
// package only exports its root `mod.ts`, which pulls `@effect/platform-node` and file watchers,
// so a browser bundle cannot take the schema without a `./normalized` subpath export upstream.

import type { ConversationItem, ToolCallItem } from './model.ts'
import { proseItem, structuredEvent } from './semantics.ts'

interface Base {
  readonly sourceId: string
  readonly sessionId?: string
  readonly timestamp: string
}

/** Encoded agent-session-ingest record accepted by the offline history adapter. */
export type IngestRecord =
  | (Omit<Base, 'timestamp'> & {
      readonly _tag: 'SessionMeta'
      readonly cwd?: string
      readonly model?: string
      readonly gitBranch?: string
      readonly tool?: string
      readonly timestamp?: string
    })
  | (Base & { readonly _tag: 'UserMessage'; readonly messageId?: string; readonly content: string })
  | (Base & {
      readonly _tag: 'AssistantText'
      readonly messageId?: string
      readonly content: string
      readonly model?: string
    })
  | (Base & { readonly _tag: 'Thinking'; readonly messageId?: string; readonly content: string })
  | (Base & {
      readonly _tag: 'ToolCallStart'
      readonly messageId?: string
      readonly toolCallId: string
      readonly toolName: string
      readonly serverName?: string
      readonly input: unknown
    })
  | (Base & {
      readonly _tag: 'ToolCallEnd'
      readonly toolCallId: string
      readonly toolName?: string
      readonly output: unknown
      readonly isError?: boolean
    })
  | (Base & {
      readonly _tag: 'StepBoundary'
      readonly kind: 'start' | 'finish'
      readonly cost?: number
      readonly tokens?: unknown
      readonly reason?: string
    })
  | (Base & { readonly _tag: 'SystemMessage'; readonly content: unknown })
  | (Omit<Base, 'timestamp'> & {
      readonly _tag: 'GenericEvent'
      readonly eventType: string
      readonly data: unknown
      readonly timestamp?: string
    })

/** opencode `tokens` (`{input, output, cache:{read}}`); other providers leave it opaque. */
const stepTokens = (tokens: unknown) => {
  const fields =
    typeof tokens === 'object' && tokens !== null ? (tokens as Record<string, unknown>) : {}
  const cache = fields['cache']
  const cached =
    typeof cache === 'object' && cache !== null
      ? (cache as Record<string, unknown>)['read']
      : undefined
  return {
    ...(typeof fields['input'] === 'number' ? { inputTokens: fields['input'] } : {}),
    ...(typeof fields['output'] === 'number' ? { outputTokens: fields['output'] } : {}),
    ...(typeof cached === 'number' ? { cachedTokens: cached } : {}),
  }
}

/**
 * Ingest records are history: nothing streams, and an unanswered tool call was interrupted.
 *
 * Thinking duration is not recorded by any provider; it is estimated as the gap since the
 * previous record ([INFERENCE] — Claude Code stamps each content block when it is written, so the
 * gap approximates time spent thinking but includes request latency).
 */
export const ingestItems = (
  records: ReadonlyArray<IngestRecord>,
): ReadonlyArray<ConversationItem> => {
  const items: Array<ConversationItem> = []
  const calls = new Map<string, number>()
  let previousAt: string | undefined
  let model: string | undefined

  records.forEach((record, index) => {
    const id = `${record.sourceId}#${index}`
    switch (record._tag) {
      case 'SessionMeta':
        model = record.model
        break
      case 'UserMessage':
        items.push(
          proseItem({
            _tag: 'Text',
            id,
            role: 'user',
            text: record.content,
            attachments: [],
            streaming: false,
            at: record.timestamp,
          }),
        )
        break
      case 'AssistantText':
        items.push({
          _tag: 'Text',
          id,
          role: 'assistant',
          text: record.content,
          attachments: [],
          streaming: false,
          ...((record.model ?? model) !== undefined ? { model: record.model ?? model } : {}),
          at: record.timestamp,
        })
        break
      case 'Thinking': {
        const durationMs =
          previousAt !== undefined
            ? Date.parse(record.timestamp) - Date.parse(previousAt)
            : undefined
        items.push({
          _tag: 'Reasoning',
          id,
          text: record.content,
          ...(durationMs !== undefined && durationMs > 0 ? { durationMs } : {}),
          streaming: false,
          at: record.timestamp,
        })
        break
      }
      case 'ToolCallStart':
        calls.set(record.toolCallId, items.length)
        items.push({
          _tag: 'ToolCall',
          id,
          callId: record.toolCallId,
          name: record.toolName,
          ...(record.serverName !== undefined ? { server: record.serverName } : {}),
          input: record.input,
          status: 'interrupted',
          callSeen: true,
          at: record.timestamp,
        })
        break
      case 'ToolCallEnd': {
        const result = {
          content: record.output,
          isError: record.isError === true,
          at: record.timestamp,
        }
        const status = record.isError === true ? 'error' : 'success'
        const callIndex = calls.get(record.toolCallId)
        const call = callIndex !== undefined ? items[callIndex] : undefined
        if (call?._tag === 'ToolCall') {
          items[callIndex!] = { ...call, status, result } satisfies ToolCallItem
        } else {
          items.push({
            _tag: 'ToolCall',
            id,
            callId: record.toolCallId,
            name: record.toolName ?? 'tool result',
            input: undefined,
            status,
            result,
            callSeen: false,
            at: record.timestamp,
          })
        }
        break
      }
      case 'StepBoundary':
        if (
          record.kind === 'finish' &&
          (record.cost !== undefined || record.tokens !== undefined)
        ) {
          items.push({
            _tag: 'Usage',
            id,
            semantics: 'response',
            ...(model !== undefined ? { model } : {}),
            ...stepTokens(record.tokens),
            ...(record.cost !== undefined ? { cost: record.cost, currency: 'USD' } : {}),
            at: record.timestamp,
          })
        }
        break
      case 'SystemMessage':
        items.push(
          typeof record.content === 'string'
            ? { _tag: 'Notice', id, kind: 'event', text: record.content, at: record.timestamp }
            : structuredEvent({
                id,
                data: record.content,
                eventType: 'system-message',
                at: record.timestamp,
              }),
        )
        break
      case 'GenericEvent':
        items.push(
          structuredEvent({
            id,
            data: record.data,
            eventType: record.eventType,
            ...(record.timestamp !== undefined ? { at: record.timestamp } : {}),
          }),
        )
        break
    }
    previousAt = record.timestamp ?? previousAt
  })
  return items
}
