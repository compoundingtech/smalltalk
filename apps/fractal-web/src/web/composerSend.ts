import type { ConversationRuntimeOptions } from '@smalltalk/fractal-ui/assistant-ui'
import type { ConversationItem, SendState } from '../conversation/model.ts'
import type { AttachmentSendRequest, DataSource, Grants } from '../data/source.ts'

export type SendRefusal = { readonly reason: string }

const sendFailureDetails: Readonly<Record<Extract<SendState, { _tag: 'Failed' }>['reason'], string>>
  & Readonly<Record<string, string | undefined>> = {
    rejected: 'The server did not accept this message.',
    ungranted: 'Message send is not granted for this device.',
    invalid: 'The message does not match the supported send format.',
    failed: 'The message could not be sent. Check your connection and retry.',
    'stale-fence': 'The conversation changed before this message could be sent. Retry to use its current state.',
    'snapshot-unavailable': 'The conversation could not be loaded for sending. Nothing was sent. Retry when it is available.',
  }

/** Send diagnostics are not display copy, including for unclassified failure reasons. */
export const sendFailureDetail = (reason: string): string =>
  (Object.hasOwn(sendFailureDetails, reason) ? sendFailureDetails[reason] : undefined) ?? sendFailureDetails.failed

/** The kit owns the draft. The data source owns keys, outbox, send state and echo reconciliation. */
export const composerSendBinding = ({ source, agentRef, grants, readable, items, refusal, onRefused }: {
  readonly source: Pick<DataSource, 'attachments'>
  readonly agentRef: string
  readonly grants: Grants
  /** The outbox shows a send only in a readable conversation; an unreadable one would hide its row and outcome. */
  readonly readable: boolean
  readonly items: NonNullable<ConversationRuntimeOptions['messages']>
  readonly refusal?: SendRefusal
  readonly onRefused: (refusal: SendRefusal) => void
}): { readonly runtime: ConversationRuntimeOptions; readonly disabledReason?: string; readonly retry: (item: ConversationItem) => Promise<void> } => {
  const disabledReason = refusal !== undefined ? sendFailureDetail(refusal.reason)
    : source.attachments === undefined ? 'This view cannot send messages.'
    : grants.messageSend !== 'granted' ? 'Message send is not granted for this device.'
    : !readable ? 'Messages can be sent once this conversation loads.' : undefined
  const dispatch = async (content: string, attachments: AttachmentSendRequest['parameters']['attachments'], retryKey?: string) => {
    if (source.attachments === undefined || grants.messageSend !== 'granted') return
    const request = {
      api_version: 'st3.client.v0', type: 'message.send', id: `action/${crypto.randomUUID()}`,
      parameters: { to: agentRef, content, tags: [], attachments },
    } as const
    const result = await source.attachments.send(retryKey === undefined
      ? { ...request, _tag: 'Send' }
      : { ...request, _tag: 'Resend', idempotencyKey: retryKey })
    // Retryable send failures belong to the outbox row, not the next draft's availability.
    if (result._tag === 'Refused' && result.reason !== 'failed'
      && result.reason !== 'stale-fence' && result.reason !== 'snapshot-unavailable') onRefused({ reason: result.reason })
  }
  return {
    ...(disabledReason === undefined ? {} : { disabledReason }),
    runtime: {
      messages: items, isDisabled: disabledReason !== undefined,
      onNew: async (message) => {
        if (disabledReason !== undefined) return
        await dispatch(message.content.filter(part => part.type === 'text').map(part => part.text).join('\n'), [])
      },
      // Deliberately no onCancel: runtime.signal is not a send-cancel contract.
    },
    retry: async (item) => {
      if (item._tag !== 'Text' || item.sendState?._tag !== 'Failed' || !item.id.startsWith('pending/')) return
      // pending/<key> is the source-owned outbox identity, not a text/time dedupe heuristic.
      const attachments: AttachmentSendRequest['parameters']['attachments'][number][] = []
      for (const attachment of item.attachments) {
        const mediaType = attachment.mediaType
        if (mediaType !== 'image/png' && mediaType !== 'image/jpeg' && mediaType !== 'image/gif' && mediaType !== 'image/webp') {
          onRefused({ reason: 'invalid' })
          return
        }
        attachments.push({ blob: attachment.id, media_type: mediaType, name: attachment.name ?? null })
      }
      await dispatch(item.text, attachments, item.id.slice('pending/'.length))
    },
  }
}
