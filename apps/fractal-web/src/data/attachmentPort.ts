import type { St3Client } from '@smalltalk/st3-client'
import type { ActionResult } from '@smalltalk/st3-client/schema'
import * as Native from '@smalltalk/st3-client/schema'

import { conversationPortFailure, hasConversationCapability } from './conversationPort.ts'
import type { AttachmentPort, ConversationPortResult, MessageSendAction } from './source.ts'

/** Upload, chunk reads and sending use the same gateway client, never a synthetic delivery. */
export const gatewayAttachments = (client: St3Client): AttachmentPort => ({
  capabilities: async () => {
    try {
      const capabilities = Native.decodeUnknownSync(Native.Capabilities)((await client.capabilities()).value)
      return {
        _tag: 'Success',
        value: {
          upload: hasConversationCapability(capabilities, 'control.messages') ? 'granted' : 'ungranted',
          send: hasConversationCapability(capabilities, 'message.send') ? 'granted' : 'ungranted',
          read: hasConversationCapability(capabilities, 'read.projections') ? 'granted' : 'ungranted',
        },
      }
    } catch (cause) {
      return conversationPortFailure(cause)
    }
  },
  upload: async ({ bytes, mediaType }) => {
    try {
      const capabilities = Native.decodeUnknownSync(Native.Capabilities)((await client.capabilities()).value)
      if (!hasConversationCapability(capabilities, 'control.messages')) {
        return { _tag: 'Refused', reason: 'ungranted', detail: 'This device is not granted attachment upload.' }
      }
      const response = await client.uploadBlob(bytes, mediaType)
      return { _tag: 'Success', value: Native.decodeUnknownSync(Native.BlobUpload)(response.value) }
    } catch (cause) {
      return conversationPortFailure(cause)
    }
  },
  chunk: async ({ sha256, ...options }) => {
    try {
      const capabilities = Native.decodeUnknownSync(Native.Capabilities)((await client.capabilities()).value)
      if (!hasConversationCapability(capabilities, 'read.projections')) {
        return { _tag: 'Refused', reason: 'ungranted', detail: 'This device is not granted attachment read access.' }
      }
      const response = await client.blobChunk(sha256, options)
      return { _tag: 'Success', value: Native.decodeUnknownSync(Native.BlobChunk)(response.value) }
    } catch (cause) {
      return conversationPortFailure(cause)
    }
  },
  send: async (request) =>
    gatewayMessageSend(client)({
      api_version: request.api_version,
      type: request.type,
      id: request.id,
      fence: request.fence,
      parameters: request.parameters,
      idempotency_key: request._tag === 'Resend' ? request.idempotencyKey : crypto.randomUUID(),
    }),
})

/**
 * Submit one message action whose idempotency key the caller already chose. Optimistic
 * senders derive the message identity from that key, so it must be single-sourced.
 */
export const gatewayMessageSend =
  (client: St3Client) =>
  async (request: MessageSendAction): Promise<ConversationPortResult<ActionResult>> => {
    let attachments: readonly Native.AttachmentInput[]
    // Validate the actual generated action contract (including max four PNG/JPEG/GIF/WebP references).
    try {
      const action = Native.decodeUnknownSync(Native.ActionRequest)(request)
      if (action.type !== 'message.send') {
        return { _tag: 'Refused', reason: 'invalid', detail: 'Attachment sending requires a message.send action.' }
      }
      // Decoding normalizes optional wire nulls; the transport takes nonnullable attachment names.
      attachments = action.parameters.attachments ?? []
    } catch (cause) {
      return { _tag: 'Refused', reason: 'invalid', detail: cause instanceof Error ? cause.message : String(cause) }
    }
    try {
      const capabilities = Native.decodeUnknownSync(Native.Capabilities)((await client.capabilities()).value)
      if (!hasConversationCapability(capabilities, 'message.send')) {
        return { _tag: 'Refused', reason: 'ungranted', detail: 'This device is not granted message send.' }
      }
      const response = await client.messageSend({
        id: request.id,
        idempotency_key: request.idempotency_key,
        fence: request.fence,
        parameters: {
          ...request.parameters,
          tags: [...request.parameters.tags],
          attachments: [...attachments],
        },
      })
      return { _tag: 'Success', value: Native.decodeUnknownSync(Native.ActionResult)(response.value) }
    } catch (cause) {
      return conversationPortFailure(cause)
    }
  }
