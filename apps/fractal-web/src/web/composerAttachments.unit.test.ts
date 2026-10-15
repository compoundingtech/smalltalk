// @vitest-environment jsdom
import { expect, it, vi } from 'vitest'
import { BlobUpload, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import type { AttachmentPort } from '../data/source.ts'
import { createComposerAttachmentAdapter } from './composerAttachments.ts'
import { composerSendBinding } from './composerSend.ts'

const file = () => new File(['image fixture'], 'fixture.png', { type: 'image/png' })
const port = () => ({ capabilities: vi.fn(), chunk: vi.fn(), send: vi.fn(), upload: vi.fn<AttachmentPort['upload']>() })
it('keeps separate runtime identities and cached native refs through retry and removal', async () => {
  const transport = port()
  const blob = `blob/${'b'.repeat(64)}`
  transport.upload.mockResolvedValue({ _tag: 'Success', value: decodeUnknownSync(BlobUpload)({
    blob, sha256: 'b'.repeat(64), size: 13, media_type: 'image/png',
  }) })
  const adapter = createComposerAttachmentAdapter(transport)
  const first = await adapter.add({ file: file() })
  const second = await adapter.add({ file: file() })
  if (!('id' in first) || !('id' in second)) throw new Error('Expected pending attachments')
  const ready = await adapter.send(first)
  await adapter.send(first)
  expect(transport.upload).toHaveBeenCalledTimes(1)
  expect(ready.id).toBe(first.id)
  expect(ready.id).not.toBe(second.id)
  expect(adapter.nativeId(ready.id)).toBe(blob)
  await adapter.remove(ready)
  expect(adapter.nativeId(ready.id)).toBeUndefined()
  for (let i = 0; i < 3; i++) await adapter.add({ file: file() })
  await expect(adapter.add({ file: file() })).rejects.toThrow('four images')
})

it('limits type, size and retained image count before uploading bytes', async () => {
  const transport = port()
  const adapter = createComposerAttachmentAdapter(transport)
  await expect(adapter.add({ file: new File(['x'], 'unsupported.svg', { type: 'image/svg+xml' }) })).rejects.toThrow('PNG')
  await expect(adapter.add({ file: new File([new Uint8Array(10 * 1024 * 1024 + 1)], 'large.png', { type: 'image/png' }) })).rejects.toThrow('10 MiB')
  for (let i = 0; i < 4; i++) await adapter.add({ file: file() })
  await expect(adapter.add({ file: file() })).rejects.toThrow('four images')
  expect(transport.upload).not.toHaveBeenCalled()
  expect(transport.send).not.toHaveBeenCalled()
})
it('failed upload retains the original file and never dispatches a message', async () => {
  const transport = port()
  transport.upload.mockResolvedValue({ _tag: 'Refused', reason: 'failed', detail: 'Native upload failed' })
  const adapter = createComposerAttachmentAdapter(transport)
  const bytes = file()
  const pending = await adapter.add({ file: bytes })
  if (!('id' in pending)) throw new Error('Expected pending attachment')
  await expect(adapter.send(pending)).rejects.toThrow('Native upload failed')
  expect(pending.file).toBe(bytes)
  expect(transport.upload).toHaveBeenCalledWith({ bytes, mediaType: 'image/png' })
  expect(transport.send).not.toHaveBeenCalled()
  await adapter.remove(pending)
  expect('id' in await adapter.add({ file: file() })).toBe(true)
})
it('serializes an image-only completed upload without silently dropping its blob', async () => {
  const transport = port()
  transport.send.mockResolvedValue({ _tag: 'Refused', reason: 'failed', detail: 'Fixture transport' })
  const binding = composerSendBinding({ source: { attachments: transport }, agentRef: 'agent/fixture', grants: { actions: 'granted', messageSend: 'granted', terminalInput: 'ungranted' }, readable: true, items: [], onRefused: vi.fn() })
  await binding.runtime.onNew({ role: 'user', content: [], attachments: [{ id: 'blob/fixture', type: 'image', name: 'fixture.png', contentType: 'image/png', status: { type: 'complete' }, content: [] }], createdAt: new Date(), metadata: { custom: {} }, parentId: null, sourceId: null, runConfig: undefined })
  expect(transport.send).toHaveBeenCalledWith(expect.objectContaining({ parameters: expect.objectContaining({ content: '', attachments: [{ blob: 'blob/fixture', media_type: 'image/png', name: 'fixture.png' }] }) }))
})
