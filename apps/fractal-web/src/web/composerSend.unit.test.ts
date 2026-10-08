import { describe, expect, it, vi } from 'vitest'
import type { AttachmentPort } from '../data/source.ts'
import { composerSendBinding, sendFailureDetail } from './composerSend.ts'

const message = {
  role: 'user', content: [{ type: 'text', text: 'hello' }], createdAt: new Date(0),
  metadata: { custom: {} }, parentId: null, sourceId: null, runConfig: undefined,
} as const
const grants = { actions: 'granted', messageSend: 'granted', terminalInput: 'ungranted' } as const
const seat = 'agent/example/scratch'

describe('composer send contract', () => {
  it('sends Send then Resend with the source key and leaves the fence to the data source', async () => {
    const send = vi.fn<AttachmentPort['send']>().mockResolvedValue({ _tag: 'Refused', reason: 'failed', detail: 'Actual refusal' })
    const onRefused = vi.fn()
    const binding = composerSendBinding({ source: { attachments: { capabilities: vi.fn(), upload: vi.fn(), chunk: vi.fn(), send } }, agentRef: seat, grants, readable: true, items: [], onRefused })
    await binding.runtime.onNew(message)
    await binding.retry({ _tag: 'Text', id: 'pending/exact-source-key', role: 'user', text: 'hello', attachments: [], streaming: false, at: new Date(0).toISOString(), sendState: { _tag: 'Failed', reason: 'failed', detail: 'Actual refusal' } })
    expect(send).toHaveBeenCalledTimes(2)
    expect(send.mock.calls[0]![0]).toMatchObject({ _tag: 'Send', parameters: { to: seat, content: 'hello' } })
    expect(send.mock.calls[1]![0]).toMatchObject({ _tag: 'Resend', idempotencyKey: 'exact-source-key' })
    expect(send.mock.calls.every(([request]) => !('fence' in request))).toBe(true)
    expect(binding.runtime.onCancel).toBeUndefined()
    expect(onRefused).not.toHaveBeenCalled()
  })

  it('disables absent ports and actual refusals instead of inventing a capability or fence', async () => {
    const onRefused = vi.fn()
    const binding = composerSendBinding({ source: {}, agentRef: seat, grants, readable: true, items: [], onRefused })
    expect(binding.disabledReason).toBe('This view cannot send messages.')
    expect(binding.runtime.isDisabled).toBe(true)
    await binding.runtime.onNew(message)
    expect(onRefused).not.toHaveBeenCalled()
    const refused = composerSendBinding({ source: {}, agentRef: seat, grants, readable: true, items: [], onRefused, refusal: { reason: 'ungranted' } })
    expect(refused.disabledReason).toBe('Message send is not granted for this device.')
    expect(refused.runtime.isDisabled).toBe(true)
  })

  it('keeps the composer but waits to send while the conversation cannot be read, under the send grant model', async () => {
    const send = vi.fn<AttachmentPort['send']>()
    const attachments = { capabilities: vi.fn(), upload: vi.fn(), chunk: vi.fn(), send }
    const unreadable = composerSendBinding({ source: { attachments }, agentRef: seat, grants, readable: false, items: [], onRefused: vi.fn() })
    expect(unreadable.disabledReason).toBe('Messages can be sent once this conversation loads.')
    expect(unreadable.runtime.isDisabled).toBe(true)
    await unreadable.runtime.onNew(message)
    expect(send).not.toHaveBeenCalled()
    // The send grant is the more fundamental refusal and keeps its own reason.
    const ungranted = composerSendBinding({ source: { attachments }, agentRef: seat, grants: { ...grants, messageSend: 'ungranted' }, readable: false, items: [], onRefused: vi.fn() })
    expect(ungranted.disabledReason).toBe('Message send is not granted for this device.')
  })

  it.each(['future-reason', 'constructor', '__proto__', 'raw-diagnostic-sentinel'])('uses generic copy for unclassified reason %s', reason => {
    expect(sendFailureDetail(reason)).toBe('The message could not be sent. Check your connection and retry.')
    const binding = composerSendBinding({ source: {}, agentRef: seat, grants, readable: true, items: [], onRefused: vi.fn(), refusal: { reason } })
    expect(binding.disabledReason).toBe('The message could not be sent. Check your connection and retry.')
  })
})
