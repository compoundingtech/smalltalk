import type { St3Client } from '@smalltalk/st3-client'
import * as Native from '@smalltalk/st3-client/schema'

import { conversationPortFailure, hasConversationCapability } from './conversationPort.ts'
import type { ContentSearchPort } from './source.ts'

/** One native search page; cursors, excerpts and indexing metadata remain the daemon's values. */
export const gatewayContentSearch = (client: St3Client): ContentSearchPort => ({
  search: async ({ text, ...options }) => {
    try {
      const capabilities = Native.decodeUnknownSync(Native.Capabilities)((await client.capabilities()).value)
      if (!hasConversationCapability(capabilities, 'read.projections') || !capabilities.session_actor.startsWith('person/')) {
        return {
          _tag: 'Refused',
          reason: 'ungranted',
          detail: 'Conversation content search requires an authenticated person with projection read access.',
        }
      }
      const response = await client.conversationSearch(text, options)
      return { _tag: 'Success', value: Native.decodeUnknownSync(Native.ConversationSearch)(response.value) }
    } catch (cause) {
      return conversationPortFailure(cause)
    }
  },
})
