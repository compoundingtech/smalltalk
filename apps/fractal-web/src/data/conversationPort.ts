import { ClientError } from '@smalltalk/st3-client'
import type { Capabilities } from '@smalltalk/st3-client/schema'

import type { ConversationPortResult } from './source.ts'

export const hasConversationCapability = (capabilities: Capabilities, id: string): boolean =>
  capabilities.capabilities.some((capability) => capability.id === id && capability.state === 'granted')

/** Keep server refusals distinct from transport/decode failures; never retry an uncertain write. */
export const conversationPortFailure = (cause: unknown): Extract<ConversationPortResult<never>, { _tag: 'Refused' }> => ({
  _tag: 'Refused',
  reason: cause instanceof ClientError && (cause.status === 401 || cause.status === 403)
    ? 'ungranted'
    : 'failed',
  detail: cause instanceof ClientError ? cause.response.message : cause instanceof Error ? cause.message : String(cause),
  ...(cause instanceof ClientError ? { error: cause.response } : {}),
})
