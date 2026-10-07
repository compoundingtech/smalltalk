/**
 * Type-only port of conversation/model.ts's Schema.Type contract. The source app owns decoding;
 * the isolated workshop does not import Effect or the generated SDK just to render synthetic data.
 * All nine source tags, readonly fields, optionality and unknown payloads are preserved here.
 */
export type Role = 'user' | 'assistant' | 'system'
export type Sender = {
  readonly kind: 'agent' | 'subagent' | 'st-agent' | 'human' | 'harness' | 'system'
  readonly label: string
  readonly via?: string
}
export type Attachment = {
  readonly id: string
  readonly mediaType: string
  readonly name?: string
}
export type ToolCallStatus = 'running' | 'success' | 'error' | 'interrupted'
export type ToolResult = {
  readonly content: unknown
  readonly mediaType?: string
  readonly isError: boolean
  readonly at: string
}
export type RunStatus = 'queued' | 'running' | 'waiting' | 'completed' | 'failed' | 'cancelled'
export type NoticeKind = 'error' | 'redaction' | 'truncation' | 'event' | 'unknown'

export type ConversationItem =
  | {
      readonly _tag: 'Text'
      readonly id: string
      readonly role: Role
      readonly text: string
      readonly attachments: readonly Attachment[]
      readonly streaming: boolean
      readonly model?: string
      readonly at: string
      readonly sender?: Sender
    }
  | {
      readonly _tag: 'Message'
      readonly id: string
      readonly messageId: string
      readonly from?: string
      readonly to?: string
      readonly title?: string
      readonly replyTo?: string
      readonly at: string
      readonly sender?: Sender
    }
  | {
      readonly _tag: 'Reasoning'
      readonly id: string
      readonly text: string
      readonly durationMs?: number
      readonly streaming: boolean
      readonly at: string
      readonly sender?: Sender
    }
  | {
      readonly _tag: 'ToolCall'
      readonly id: string
      readonly callId: string
      readonly name: string
      readonly server?: string
      readonly input: unknown
      readonly status: ToolCallStatus
      readonly result?: ToolResult
      readonly callSeen: boolean
      readonly at: string
      readonly sender?: Sender
    }
  | {
      readonly _tag: 'Status'
      readonly id: string
      readonly status: RunStatus
      readonly detail?: string
      readonly at: string
    }
  | {
      readonly _tag: 'Usage'
      readonly id: string
      readonly semantics: 'context_occupancy' | 'session_cumulative' | 'response'
      readonly model?: string
      readonly inputTokens?: number
      readonly outputTokens?: number
      readonly cachedTokens?: number
      readonly cost?: number
      readonly currency?: string
      readonly contextUsedPercent?: number
      readonly at: string
    }
  | {
      readonly _tag: 'Notice'
      readonly id: string
      readonly kind: NoticeKind
      readonly text: string
      readonly detail?: string
      readonly retryable?: boolean
      readonly at?: string
      readonly sender?: Sender
    }
  | {
      readonly _tag: 'Event'
      readonly id: string
      readonly kind: 'resource-observed' | 'harness-message' | 'subagent-result'
      readonly title: string
      readonly text: string
      readonly sender: Sender
      readonly data: unknown
      readonly at?: string
    }
  | {
      readonly _tag: 'UnknownEvent'
      readonly id: string
      readonly eventType: string
      readonly data: unknown
      readonly sender?: Sender
      readonly at?: string
    }

export type TextItem = Extract<ConversationItem, { _tag: 'Text' }>
export type MessageItem = Extract<ConversationItem, { _tag: 'Message' }>
export type ReasoningItem = Extract<ConversationItem, { _tag: 'Reasoning' }>
export type ToolCallItem = Extract<ConversationItem, { _tag: 'ToolCall' }>
export type StatusItem = Extract<ConversationItem, { _tag: 'Status' }>
export type UsageItem = Extract<ConversationItem, { _tag: 'Usage' }>
export type NoticeItem = Extract<ConversationItem, { _tag: 'Notice' }>
export type EventItem = Extract<ConversationItem, { _tag: 'Event' }>
export type UnknownEventItem = Extract<ConversationItem, { _tag: 'UnknownEvent' }>
