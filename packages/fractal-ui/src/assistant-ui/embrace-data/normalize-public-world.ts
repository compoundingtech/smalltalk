import type { ConversationItem, Sender, ToolCallItem } from './model.ts'
import type { PublicConversation } from './public-world.ts'

/**
 * Narrow port of fromTimeline.ts's message/content/tool-call/result fold for the public generator's
 * four wire entry types. Results join calls, rather than becoming authored assistant messages.
 * Unlike SDK decoding, public JSON timestamps already are ISO strings. Worker/operator context
 * supplies provenance because the public wire projection does not encode a `sender` field.
 */
export const normalizePublicConversation = (conversation: PublicConversation): readonly ConversationItem[] => {
  const items: ConversationItem[] = []
  const calls = new Map<string, number>()
  const worker: Sender = { kind: 'agent', label: conversation.agentLabel }
  const operator: Sender = { kind: 'human', label: 'Operator' }
  for (const entry of conversation.items) {
    switch (entry.type) {
      case 'message':
        items.push({
          _tag: 'Message', id: entry.id, messageId: entry.body.message_id,
          from: entry.body.from, to: entry.body.to, title: entry.body.title,
          ...(entry.body.reply_to === null ? {} : { replyTo: entry.body.reply_to }),
          at: entry.timestamp, sender: operator,
        })
        break
      case 'content':
        items.push({
          _tag: 'Text', id: entry.id, role: entry.role === 'tool' ? 'system' : entry.role,
          text: entry.body.text, attachments: [], streaming: !entry.final, at: entry.timestamp,
          sender: entry.role === 'user' ? operator : worker,
        })
        break
      case 'tool_call':
        calls.set(entry.body.call_id, items.length)
        items.push({
          _tag: 'ToolCall', id: entry.id, callId: entry.body.call_id, name: entry.body.name,
          input: entry.body.arguments, status: 'running', callSeen: true,
          at: entry.timestamp, sender: worker,
        })
        break
      case 'tool_result': {
        const index = calls.get(entry.body.call_id)
        const call = index === undefined ? undefined : items[index]
        const result: ToolCallItem = {
          ...(call?._tag === 'ToolCall' ? call : {
            _tag: 'ToolCall', id: entry.id, callId: entry.body.call_id, name: 'tool result',
            input: undefined, callSeen: false, at: entry.timestamp, sender: worker,
          }),
          status: entry.body.status,
          result: {
            content: entry.body.content, mediaType: entry.body.media_type,
            isError: entry.body.status === 'error', at: entry.timestamp,
          },
        }
        if (index === undefined) items.push(result)
        else items[index] = result
        break
      }
      default: {
        const unhandled: never = entry
        throw new Error(`Unrecognized public timeline entry: ${unhandled}`)
      }
    }
  }
  return items
}
