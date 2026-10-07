// moves to smalltalk/clients/typescript
//
// The normalized conversation view model. Both sources fold into it: the live st3 conversation
// follow (generated `TimelineEntry`, see ./fromTimeline.ts) and offline harness history
// (`@overeng/agent-session-ingest` `NormalizedRecord`, see ./ingest.ts + ./fromIngest.ts).
// Renderers only ever see `ConversationItem`; the case set follows the draft in
// context/agent-ecosystem/03-coding-agents/08-clients/02-webfractal/02-conversation/spec.md#items.

import { Schema } from 'effect'

/** Participant roles displayed by the normalized conversation renderer. */
export const Role = Schema.Literals(['user', 'assistant', 'system'])
export type Role = typeof Role.Type

/** Sender provenance is independent of the model role (IRC and deliveries are not human prompts). */
export const Sender = Schema.Struct({
  kind: Schema.Literals(['agent', 'subagent', 'st-agent', 'human', 'harness', 'system']),
  label: Schema.String,
  via: Schema.optional(Schema.String),
})
export type Sender = typeof Sender.Type

/** A user-supplied file or image. st only carries an opaque `attachment_id` today (open Q3). */
export const Attachment = Schema.Struct({
  id: Schema.String,
  mediaType: Schema.String,
  name: Schema.optional(Schema.String),
})
export type Attachment = typeof Attachment.Type

/**
 * `interrupted`: no result arrived and the session is no longer running — distinct from
 * `running` so a finished transcript never shows a spinner forever.
 */
export const ToolCallStatus = Schema.Literals(['running', 'success', 'error', 'interrupted'])
export type ToolCallStatus = typeof ToolCallStatus.Type

/** Tool output joined to its invocation, including media type and completion time. */
export const ToolResult = Schema.Struct({
  content: Schema.Unknown,
  mediaType: Schema.optional(Schema.String),
  isError: Schema.Boolean,
  at: Schema.String,
})
export type ToolResult = typeof ToolResult.Type

/** Lifecycle states reported by the conversation's current run. */
export const RunStatus = Schema.Literals([
  'queued',
  'running',
  'waiting',
  'completed',
  'failed',
  'cancelled',
])
export type RunStatus = typeof RunStatus.Type

/** Notice categories supported by the compact transcript renderer. */
export const NoticeKind = Schema.Literals(['error', 'redaction', 'truncation', 'event', 'unknown'])
export type NoticeKind = typeof NoticeKind.Type

/** Shared renderer contract for live timeline entries and imported session history. */
export const ConversationItem = Schema.TaggedUnion({
  /** Prose from a participant. `streaming` = the entry is not final yet (revisions replace it). */
  Text: {
    id: Schema.String,
    role: Role,
    text: Schema.String,
    attachments: Schema.Array(Attachment),
    streaming: Schema.Boolean,
    model: Schema.optional(Schema.String),
    at: Schema.String,
    sender: Schema.optional(Sender),
  },
  /** A Small Talk message envelope joined into the conversation (st `message` entry). */
  Message: {
    id: Schema.String,
    messageId: Schema.String,
    from: Schema.optional(Schema.String),
    to: Schema.optional(Schema.String),
    title: Schema.optional(Schema.String),
    replyTo: Schema.optional(Schema.String),
    at: Schema.String,
    sender: Schema.optional(Sender),
  },
  /** Model reasoning. No st wire source yet (open Q2); ingest `Thinking` and proposed D08 feed it. */
  Reasoning: {
    id: Schema.String,
    text: Schema.String,
    durationMs: Schema.optional(Schema.Finite),
    streaming: Schema.Boolean,
    at: Schema.String,
    sender: Schema.optional(Sender),
  },
  /** A tool invocation with its joined result (CAG.CLI.WEB.CNV-R04). `callSeen: false` = orphan result. */
  ToolCall: {
    id: Schema.String,
    callId: Schema.String,
    name: Schema.String,
    server: Schema.optional(Schema.String),
    input: Schema.Unknown,
    status: ToolCallStatus,
    result: Schema.optional(ToolResult),
    callSeen: Schema.Boolean,
    at: Schema.String,
    sender: Schema.optional(Sender),
  },
  /** Run lifecycle heartbeat; the renderer folds these into the tail state. */
  Status: {
    id: Schema.String,
    status: RunStatus,
    detail: Schema.optional(Schema.String),
    at: Schema.String,
  },
  /** Token/cost accounting; rendered as footer metadata on the preceding assistant turn. */
  Usage: {
    id: Schema.String,
    semantics: Schema.Literals(['context_occupancy', 'session_cumulative', 'response']),
    model: Schema.optional(Schema.String),
    inputTokens: Schema.optional(Schema.Finite),
    outputTokens: Schema.optional(Schema.Finite),
    cachedTokens: Schema.optional(Schema.Finite),
    cost: Schema.optional(Schema.Finite),
    currency: Schema.optional(Schema.String),
    contextUsedPercent: Schema.optional(Schema.Finite),
    at: Schema.String,
  },
  /** Compact one-line notice: error, redaction, truncation, cleaned event, unknown entry type. */
  Notice: {
    id: Schema.String,
    kind: NoticeKind,
    text: Schema.String,
    detail: Schema.optional(Schema.String),
    retryable: Schema.optional(Schema.Boolean),
    at: Schema.optional(Schema.String),
    sender: Schema.optional(Sender),
  },
  /** Known structured harness events, never displayed as a raw JSON transcript. */
  Event: {
    id: Schema.String,
    kind: Schema.Literals(['resource-observed', 'harness-message', 'subagent-result']),
    title: Schema.String,
    text: Schema.String,
    sender: Sender,
    data: Schema.Unknown,
    at: Schema.optional(Schema.String),
  },
  /** Forward-compatible compact row with explicit type and an inspectable structured payload. */
  UnknownEvent: {
    id: Schema.String,
    eventType: Schema.String,
    data: Schema.Unknown,
    sender: Schema.optional(Sender),
    at: Schema.optional(Schema.String),
  },
})
export type ConversationItem = typeof ConversationItem.Type

/** Participant prose, attachments, and streaming state. */
export type TextItem = (typeof ConversationItem.cases.Text)['Type']
/** Small Talk message envelope embedded in the transcript. */
export type MessageItem = (typeof ConversationItem.cases.Message)['Type']
/** Model reasoning text and optional elapsed duration. */
export type ReasoningItem = (typeof ConversationItem.cases.Reasoning)['Type']
/** Tool invocation and its joined completion result. */
export type ToolCallItem = (typeof ConversationItem.cases.ToolCall)['Type']
/** Run lifecycle update used to derive the transcript tail. */
export type StatusItem = (typeof ConversationItem.cases.Status)['Type']
/** Token and cost accounting associated with an assistant turn. */
export type UsageItem = (typeof ConversationItem.cases.Usage)['Type']
/** Compact error, redaction, truncation, or informational transcript notice. */
export type NoticeItem = (typeof ConversationItem.cases.Notice)['Type']
/** Structured harness event with an intentional semantic renderer. */
export type EventItem = (typeof ConversationItem.cases.Event)['Type']
/** Unsupported event payload, retained for inspection without polluting the transcript. */
export type UnknownEventItem = (typeof ConversationItem.cases.UnknownEvent)['Type']
