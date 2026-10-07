import type { MessageStatus, ThreadMessageLike } from '@assistant-ui/react'
import type { ConversationItem, Sender, UsageItem } from './embrace-data/model.ts'

export type { ConversationItem } from './embrace-data/model.ts'

export type ConverterDiagnostic =
  | 'opaque-attachments-retained'
  | 'tool-input-not-json'
  | 'tool-input-not-object'
  | 'system-streaming-retained'
  | 'semantic-row-as-text'

export type EmbraceMessageMetadata = {
  readonly item: ConversationItem
  readonly sender: Sender | undefined
  readonly participantKey: string
  readonly diagnostics: readonly ConverterDiagnostic[]
}

/** Provenance groups agents independently of their model role; transport is not participant identity. */
export const participantKeyForItem = (item: ConversationItem): string => {
  const sender = 'sender' in item ? item.sender : undefined
  if (sender !== undefined) return `${sender.kind}:${sender.label}`
  if (item._tag === 'Message' && item.from !== undefined) return item.from
  return item._tag === 'Text' ? `role:${item.role}` : 'system:transcript'
}

const completed: MessageStatus = { type: 'complete', reason: 'stop' }
const running: MessageStatus = { type: 'running' }
const cancelled: MessageStatus = { type: 'incomplete', reason: 'cancelled' }

const usageSummary = (item: UsageItem): string => {
  const fields = [
    item.inputTokens === undefined ? undefined : `${item.inputTokens.toLocaleString('en-US')} input`,
    item.outputTokens === undefined ? undefined : `${item.outputTokens.toLocaleString('en-US')} output`,
    item.cachedTokens === undefined ? undefined : `${item.cachedTokens.toLocaleString('en-US')} cached`,
    item.cost === undefined ? undefined : `${item.cost} ${item.currency ?? 'cost units'}`,
    item.contextUsedPercent === undefined ? undefined : `${item.contextUsedPercent}% context`,
    item.model,
  ].filter((field) => field !== undefined)
  return `Usage · ${item.semantics.replaceAll('_', ' ')}${fields.length ? ` · ${fields.join(' · ')}` : ''}`
}

/**
 * The converter is the production-shaped external-store seam, not a second authored message list.
 * Read against installed @assistant-ui/react 0.15.25 / core 0.3.24: only assistant messages carry
 * status, only user messages carry attachments, and system messages contain exactly one text part.
 * Lossy display projections always retain the original item by reference in metadata.custom.
 */
export const convertConversationItem = (item: ConversationItem): ThreadMessageLike => {
  const sender = 'sender' in item ? item.sender : undefined
  const diagnostics: ConverterDiagnostic[] = []
  const metadata: { custom: EmbraceMessageMetadata } = {
    custom: { item, sender, participantKey: participantKeyForItem(item), diagnostics },
  }
  const common = {
    id: item.id,
    // Missing optional timestamps must not become nondeterministic `new Date()` in the runtime.
    createdAt: new Date(item.at ?? 0),
    metadata,
  }
  switch (item._tag) {
    case 'Text': {
      const text = { type: 'text', id: `${item.id}:text`, text: item.text } as const
      if (item.attachments.length > 0) diagnostics.push('opaque-attachments-retained')
      if (item.role === 'user') return {
        ...common, role: 'user', content: [text],
        attachments: item.attachments.map((attachment): NonNullable<ThreadMessageLike['attachments']>[number] => ({
          id: attachment.id,
          type: attachment.mediaType.startsWith('image/') ? 'image' : 'file',
          name: attachment.name ?? attachment.id,
          contentType: attachment.mediaType,
          status: { type: 'complete' },
          // The source has only an opaque id, not bytes/a URL. Do not invent preview contents.
          content: [],
        })),
      }
      if (item.role === 'system') {
        if (item.streaming) diagnostics.push('system-streaming-retained')
        return { ...common, role: 'system', content: [text] }
      }
      return {
        ...common, role: 'assistant', status: item.streaming ? running : completed,
        content: [{ ...text, status: { type: item.streaming ? 'running' : 'complete' } }],
      }
    }
    case 'Reasoning':
      return {
        ...common, role: 'assistant', status: item.streaming ? running : completed,
        content: [{
          type: 'reasoning', id: `${item.id}:reasoning`, text: item.text,
          status: { type: item.streaming ? 'running' : 'complete' },
        }],
      }
    case 'ToolCall': {
      let argsText = ''
      try {
        argsText = JSON.stringify(item.input) ?? ''
        if (argsText === '') diagnostics.push('tool-input-not-json')
      } catch {
        // Unknown payloads can be non-JSON; retain them in `item`, never stringify raw transcript data.
        diagnostics.push('tool-input-not-json')
      }
      if (typeof item.input !== 'object' || item.input === null || Array.isArray(item.input)) {
        diagnostics.push('tool-input-not-object')
      }
      const status: MessageStatus = item.status === 'running' ? running
        : item.status === 'interrupted' ? cancelled
        : item.status === 'error' ? {
            type: 'incomplete', reason: 'error', error: { message: `${item.name} failed` },
          }
        : completed
      return {
        ...common, role: 'assistant', status,
        content: [{
          type: 'tool-call', toolCallId: item.callId, toolName: item.name, argsText,
          ...(item.result === undefined ? {} : { result: item.result.content }),
          isError: item.status === 'error' || item.result?.isError === true,
          // Unlike a human interrupt, a source interrupted call is terminal, with no invented result.
        }],
      }
    }
    case 'Message': {
      diagnostics.push('semantic-row-as-text')
      const role = sender?.kind === 'human' ? 'user'
        : sender?.kind === 'agent' || sender?.kind === 'subagent' || sender?.kind === 'st-agent'
          ? 'assistant' : 'system'
      return {
        ...common, role,
        ...(role === 'assistant' ? { status: completed } : {}),
        content: [{ type: 'text', text: item.title ?? `Message ${item.messageId}` }],
      }
    }
    case 'Status':
      diagnostics.push('semantic-row-as-text')
      return {
        ...common, role: 'system',
        content: [{ type: 'text', text: `Run ${item.status}${item.detail ? ` · ${item.detail}` : ''}` }],
      }
    case 'Usage':
      diagnostics.push('semantic-row-as-text')
      return { ...common, role: 'system', content: [{ type: 'text', text: usageSummary(item) }] }
    case 'Notice':
      diagnostics.push('semantic-row-as-text')
      return item.kind === 'error' ? {
        ...common, role: 'assistant',
        status: { type: 'incomplete', reason: 'error', error: { message: item.text } },
        content: [{ type: 'text', text: `${item.text}${item.detail ? `\n${item.detail}` : ''}` }],
      } : {
        ...common, role: 'system',
        content: [{ type: 'text', text: `${item.kind} · ${item.text}${item.detail ? `\n${item.detail}` : ''}` }],
      }
    case 'Event': {
      diagnostics.push('semantic-row-as-text')
      const role = item.sender.kind === 'human' ? 'user'
        : item.sender.kind === 'agent' || item.sender.kind === 'subagent' || item.sender.kind === 'st-agent'
          ? 'assistant' : 'system'
      return {
        ...common, role,
        ...(role === 'assistant' ? { status: completed } : {}),
        content: [{ type: 'text', text: `${item.title}\n${item.text}` }],
      }
    }
    case 'UnknownEvent':
      diagnostics.push('semantic-row-as-text')
      return {
        ...common, role: 'system',
        content: [{ type: 'text', text: `Unsupported event · ${item.eventType}` }],
      }
    default: {
      const unhandled: never = item
      throw new Error(`Unrecognized ConversationItem: ${unhandled}`)
    }
  }
}
