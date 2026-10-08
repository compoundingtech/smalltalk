import { St3Client } from '@smalltalk/st3-client'
import * as Native from '@smalltalk/st3-client/schema'
import { Schema } from 'effect'
import { describe, expect, it } from 'vitest'

import { gatewayAttachments } from './attachmentPort.ts'
import type { AttachmentSendRequest } from './source.ts'
import recording from './subjectReadPort.gateway.fixtures.json' with { type: 'json' }

const sha256 = 'b'.repeat(64)
const upload = { blob: `blob/${sha256}`, sha256, size: 731, media_type: 'image/webp' }
const chunk = { sha256, size: 731, offset: 512, data: 'cmVhbCB0YWls' }
const request: AttachmentSendRequest = {
  api_version: 'st3.client.v0', type: 'message.send', id: 'action/image-send',
  idempotency_key: 'image-send-stable-key',
  fence: { snapshot_id: 'snapshot/user-observed', subject_revisions: {} },
  parameters: {
    to: 'agent/recipient', session_id: 'session/current', content: 'Here is the image',
    title: 'Actual title', in_reply_to: 'message/parent', tags: ['actual-tag'],
    attachments: [{ blob: upload.blob, media_type: 'image/webp', name: 'paste.webp' }],
  },
}
const ack = {
  kind: 'action-result', action_id: 'action/returned', operation_id: 'operation/returned',
  snapshot_id: 'snapshot/returned', affected_ids: ['message/returned'], status: 'accepted',
}

const attachmentClient = ({
  states = {},
  reply = (url: URL) => Response.json({
    api_version: 'st3.client.v0', snapshot: { id: 'snapshot/native' },
    value: url.pathname.endsWith('/chunk') ? chunk : url.pathname.endsWith('/actions') ? ack : upload,
  }),
}: {
  readonly states?: Readonly<Record<string, string>>
  readonly reply?: (url: URL) => Response | Promise<Response>
} = {}) => {
  const calls: { readonly url: URL; readonly init?: RequestInit }[] = []
  const client = new St3Client({
    baseUrl: 'https://gateway.invalid',
    fetchImpl: async (input, init) => {
      const url = new URL(String(input))
      calls.push({ url, ...(init === undefined ? {} : { init }) })
      return url.pathname.endsWith('/capabilities')
        ? Response.json({ ...recording.capabilities, value: {
          ...recording.capabilities.value,
          capabilities: ['control.messages', 'message.send', 'read.projections'].map((id) => ({
            id, version: 0, state: states[id] ?? 'granted',
          })),
        } })
        : reply(url)
    },
  })
  return { port: gatewayAttachments(client), calls }
}

describe('native attachment port', () => {
  it('exposes independent upload, send and read capability for composer attach/paste', async () => {
    const { port } = attachmentClient({ states: { 'control.messages': 'granted', 'message.send': 'ungranted', 'read.projections': 'unsupported' } })
    expect(await port.capabilities()).toEqual({ _tag: 'Success', value: { upload: 'granted', send: 'ungranted', read: 'ungranted' } })
  })

  it('uploads the exact binary body/media type and returns the real blob, hash, size and media type without sending', async () => {
    const { port, calls } = attachmentClient()
    const bytes = new Uint8Array([82, 73, 70, 70, 1, 2, 3])
    expect(await port.upload({ bytes, mediaType: 'image/webp' })).toEqual({ _tag: 'Success', value: upload })
    expect(calls.map(({ url }) => url.pathname)).toEqual(['/v1/client/capabilities', '/v1/client/blobs'])
    expect(calls[1]?.init?.method).toBe('POST')
    expect(calls[1]?.init?.body).toBe(bytes)
    expect(new Headers(calls[1]?.init?.headers).get('Content-Type')).toBe('image/webp')
  })

  it('reads the requested native chunk with the carrier message and offset unchanged', async () => {
    const { port, calls } = attachmentClient()
    expect(await port.chunk({ sha256, message: 'message/carrier & exact', offset: 512 })).toEqual({ _tag: 'Success', value: chunk })
    expect(calls[1]?.url.pathname).toBe(`/v1/client/blobs/${sha256}/chunk`)
    expect(calls[1]?.url.searchParams.get('message')).toBe('message/carrier & exact')
    expect(calls[1]?.url.searchParams.get('offset')).toBe('512')
    expect(calls[1]?.init?.method).toBe('GET')
  })

  it.each(['accepted', 'completed', 'rejected'])('preserves the real %s acknowledgement and sends all request identities/references unchanged', async (status) => {
    const actual = { ...ack, status }
    const { port, calls } = attachmentClient({ reply: () => Response.json({ api_version: 'st3.client.v0', snapshot: {}, value: actual }) })
    expect(await port.send(request)).toEqual({ _tag: 'Success', value: Native.decodeUnknownSync(Native.ActionResult)(actual) })
    expect(calls.map(({ url }) => url.pathname)).toEqual(['/v1/client/capabilities', '/v1/client/actions'])
    expect(calls[1]?.init?.method).toBe('POST')
    expect(Schema.decodeUnknownSync(Schema.fromJsonString(Schema.Unknown))(calls[1]?.init?.body)).toEqual(request)
  })

  it('sends the generated maximum of four supported image references', async () => {
    const mediaTypes = ['image/png', 'image/jpeg', 'image/gif', 'image/webp'] as const
    const attachments = mediaTypes.map((media_type, index) => ({ blob: `blob/${String(index).repeat(64)}`, media_type, name: `image-${index}` }))
    const action = { ...request, parameters: { ...request.parameters, attachments } }
    const { port, calls } = attachmentClient()
    expect(await port.send(action)).toMatchObject({ _tag: 'Success' })
    expect(Schema.decodeUnknownSync(Schema.fromJsonString(Schema.Unknown))(calls[1]?.init?.body)).toEqual(action)
  })

  it('uses decoded attachment inputs so an optional wire-null name is not sent to the transport', async () => {
    const { port, calls } = attachmentClient()
    const attachment = { blob: upload.blob, media_type: 'image/webp' as const }
    const action = { ...request, parameters: { ...request.parameters, attachments: [{ ...attachment, name: null }] } }
    expect(await port.send(action)).toMatchObject({ _tag: 'Success' })
    expect(Schema.decodeUnknownSync(Schema.fromJsonString(Schema.Unknown))(calls[1]?.init?.body))
      .toEqual({ ...action, parameters: { ...action.parameters, attachments: [attachment] } })
  })

  it('rejects five attachments through the generated schema before contacting the gateway', async () => {
    const { port, calls } = attachmentClient()
    const attachments = Array.from({ length: 5 }, () => ({ blob: upload.blob, media_type: 'image/webp' as const }))
    expect(await port.send({ ...request, parameters: { ...request.parameters, attachments } })).toMatchObject({ _tag: 'Refused', reason: 'invalid' })
    expect(calls).toHaveLength(0)
  })

  it('rejects a malformed blob reference through the generated schema', async () => {
    const { port, calls } = attachmentClient()
    const attachments = [{ blob: 'blob/invented', media_type: 'image/png' as const }]
    expect(await port.send({ ...request, parameters: { ...request.parameters, attachments } })).toMatchObject({ _tag: 'Refused', reason: 'invalid' })
    expect(calls).toHaveLength(0)
  })

  it.each(['ungranted', 'unsupported', 'unavailable'])('denies %s upload access without uploading', async (state) => {
    const { port, calls } = attachmentClient({ states: { 'control.messages': state } })
    expect(await port.upload({ bytes: new Uint8Array([1]), mediaType: 'image/png' })).toMatchObject({ _tag: 'Refused', reason: 'ungranted' })
    expect(calls).toHaveLength(1)
  })

  it('does not let upload or broad message-control permission authorize a message send', async () => {
    const { port, calls } = attachmentClient({ states: { 'message.send': 'ungranted' } })
    expect(await port.send(request)).toMatchObject({ _tag: 'Refused', reason: 'ungranted' })
    expect(calls).toHaveLength(1)
  })

  it('denies chunk access without projection read permission', async () => {
    const { port, calls } = attachmentClient({ states: { 'read.projections': 'ungranted' } })
    expect(await port.chunk({ sha256, message: 'message/carrier' })).toMatchObject({ _tag: 'Refused', reason: 'ungranted' })
    expect(calls).toHaveLength(1)
  })

  it('refreshes permissions after capability discovery rather than trusting a stale grant', async () => {
    const states: Record<string, string> = {}
    const { port, calls } = attachmentClient({ states })
    expect(await port.capabilities()).toMatchObject({ _tag: 'Success', value: { upload: 'granted' } })
    states['control.messages'] = 'ungranted'
    expect(await port.upload({ bytes: new Uint8Array([1]), mediaType: 'image/png' })).toMatchObject({ _tag: 'Refused', reason: 'ungranted' })
    expect(calls.map(({ url }) => url.pathname)).toEqual(['/v1/client/capabilities', '/v1/client/capabilities'])
  })

  it('preserves the server refusal envelope and never substitutes delivery', async () => {
    const error = {
      api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', code: 'forbidden',
      message: 'Credential cannot send attachments', retryable: false,
      request_id: 'request/native-refusal', details: { scope: 'control.messages' },
    }
    const { port, calls } = attachmentClient({ reply: () => Response.json(error, { status: 403 }) })
    expect(await port.send(request)).toEqual({ _tag: 'Refused', reason: 'ungranted', detail: error.message, error })
    expect(calls).toHaveLength(2)
  })

  it('never retries an uncertain send or claims it was delivered', async () => {
    const { port, calls } = attachmentClient({ reply: () => Promise.reject(new Error('Connection lost after submit')) })
    expect(await port.send(request)).toEqual({ _tag: 'Refused', reason: 'failed', detail: 'Connection lost after submit' })
    expect(calls).toHaveLength(2)
  })

  it('does not expose a malformed upload response as a usable image reference', async () => {
    const { port } = attachmentClient({ reply: () => Response.json({ api_version: 'st3.client.v0', snapshot: {}, value: { ...upload, media_type: 'image/svg+xml' } }) })
    expect(await port.upload({ bytes: new Uint8Array([1]), mediaType: 'image/png' })).toMatchObject({ _tag: 'Refused', reason: 'failed' })
  })
})
