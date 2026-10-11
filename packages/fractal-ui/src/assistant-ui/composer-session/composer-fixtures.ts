import { createMessageQueue, MessageNotSentError, SimpleImageAttachmentAdapter, type AppendMessage } from '@assistant-ui/react'
import type { ConversationRuntimeOptions } from '../EmbraceRuntime'
import type { ConversationItem } from '../embrace-data/model'
import { messageEffort, unknownEffort, type EffortControl } from '../embrace-composer/EffortPicker'

export const fixtureEffortControls = {
  supported: { state: 'supported', values: ['low', 'medium', 'high'], default: 'medium' },
  unsupported: { state: 'unsupported', reason: 'This harness does not support per-message effort.' },
  unknown: unknownEffort,
} as const satisfies Record<string, EffortControl>

export const composerStates = ['idle', 'typing multi-line', 'running', 'queued x2', 'failed send (restored)', 'offline draft', 'image attached'] as const
export type ComposerState = typeof composerStates[number]
export interface ComposerTarget { readonly ref: string; readonly label: string; readonly model?: string }
export interface ComposerReceipt { readonly message: AppendMessage; readonly target?: ComposerTarget; readonly effort?: string; readonly mode: 'send' | 'steer' }
export interface ComposerFixtureSnapshot {
  readonly options: ConversationRuntimeOptions
  readonly receipts: readonly ComposerReceipt[]
  readonly offline: boolean
  readonly failure?: string
  readonly effortControl: EffortControl
  readonly queuedEffort: ReadonlyMap<string, string>
}
export interface ComposerFixture {
  readonly getSnapshot: () => ComposerFixtureSnapshot
  readonly subscribe: (notify: () => void) => () => void
  readonly initialDraft: string
  readonly imageFile?: File
  readonly finish: () => void
  readonly reconnect: () => void
}
const imageAdapter = new SimpleImageAttachmentAdapter()
const image = '<svg xmlns="http://www.w3.org/2000/svg" width="360" height="180" viewBox="0 0 360 180"><rect width="360" height="180" fill="#151515"/><rect x="20" y="24" width="136" height="132" rx="8" fill="#326af1"/><rect x="180" y="24" width="160" height="32" rx="6" fill="#eeeeee"/><rect x="180" y="72" width="130" height="12" rx="6" fill="#aaaaaa"/><rect x="180" y="100" width="160" height="12" rx="6" fill="#aaaaaa"/><rect x="180" y="128" width="112" height="12" rx="6" fill="#aaaaaa"/></svg>'
const append = (text: string, effort?: string): AppendMessage => ({ role: 'user', content: [{ type: 'text', text }], attachments: [], createdAt: new Date(), metadata: { custom: {} }, parentId: null, sourceId: null, runConfig: effort === undefined ? undefined : { custom: { effort } } })

/** Explicit in-memory fixture operations; no network/gateway transport is claimed. */
export function createComposerFixture(state: ComposerState, effortControl: EffortControl = unknownEffort): ComposerFixture {
  const listeners = new Set<() => void>()
  let messages: readonly ConversationItem[] = []
  let receipts: readonly ComposerReceipt[] = []
  let offline = state === 'offline draft'
  let running = state === 'running' || state === 'queued x2'
  let rejectNext = state === 'failed send (restored)'
  let failure: string | undefined
  let sequence = 0
  let queuedEffort: ReadonlyMap<string, string> = new Map()
  let snapshot: ComposerFixtureSnapshot
  const publish = () => {
    snapshot = { options: { messages, isRunning: running, isSendDisabled: offline, onNew, onCancel, queue: queueAdapter, adapters }, receipts, offline, failure, effortControl, queuedEffort }
    for (const notify of listeners) notify()
  }
  const dispatch = (message: AppendMessage, steer: boolean) => {
    if (offline || rejectNext) {
      rejectNext = false
      failure = offline ? 'Offline. The draft remains local until you reconnect.' : 'The fixture send was rejected before acceptance. Your draft was restored.'
      publish()
      throw new MessageNotSentError(failure)
    }
    failure = undefined
    const target = message.runConfig?.custom?.explorerTarget as ComposerTarget | undefined
    receipts = [...receipts, { message, target, effort: messageEffort(message.runConfig?.custom), mode: steer ? 'steer' : 'send' }]
    messages = [...messages, { _tag: 'Text', id: `composer-fixture-${++sequence}`, role: message.role, text: message.content.filter(part => part.type === 'text').map(part => part.text).join('\n'), attachments: (message.attachments ?? []).map(attachment => ({ id: attachment.id, mediaType: attachment.contentType ?? 'application/octet-stream', name: attachment.name })), streaming: false, at: message.createdAt.toISOString(), model: target?.model, sender: { kind: 'human', label: 'You' } }]
    running = true
    queue.notifyBusy()
    publish()
  }
  const finish = () => { running = false; publish(); queue.notifyIdle() }
  const queue = createMessageQueue({ run: (message, { steer }) => dispatch(message, steer), cancel: finish })
  // Native queue snapshots omit runConfig. Associate display metadata by the new item ID,
  // never by prompt text; the native queue remains the sole owner of ordering/delivery.
  const captureEffort = (message: AppendMessage, enqueue: () => void) => {
    const before = new Set([...queue.adapter.items, ...queue.adapter.steerItems].map(item => item.id))
    enqueue()
    const effort = messageEffort(message.runConfig?.custom)
    if (effort === undefined) return
    const next = new Map(queuedEffort)
    for (const item of [...queue.adapter.items, ...queue.adapter.steerItems]) if (!before.has(item.id)) next.set(item.id, effort)
    queuedEffort = next
    publish()
  }
  const queueAdapter: NonNullable<ConversationRuntimeOptions['queue']> = {
    get items() { return queue.adapter.items },
    get steerItems() { return queue.adapter.steerItems },
    enqueue: message => captureEffort(message, () => queue.adapter.enqueue(message)),
    steer: message => captureEffort(message, () => queue.adapter.steer(message)),
    move: queue.adapter.move,
    edit: (id, message) => {
      const next = new Map(queuedEffort)
      const effort = messageEffort(message.runConfig?.custom)
      if (effort === undefined) next.delete(id); else next.set(id, effort)
      queuedEffort = next
      queue.adapter.edit(id, message)
      publish()
    },
    remove: id => { const next = new Map(queuedEffort); next.delete(id); queuedEffort = next; queue.adapter.remove(id); publish() },
    __internal_setDispatchTransform: queue.adapter.__internal_setDispatchTransform,
    __internal_notifyCancelled: queue.adapter.__internal_notifyCancelled,
  }
  queue.subscribe(() => {
    const ids = new Set([...queue.adapter.items, ...queue.adapter.steerItems].map(item => item.id))
    queuedEffort = new Map([...queuedEffort].filter(([id]) => ids.has(id)))
    publish()
  })
  const adapters = { attachments: imageAdapter }
  const onNew: ConversationRuntimeOptions['onNew'] = async message => dispatch(message, false)
  const onCancel: NonNullable<ConversationRuntimeOptions['onCancel']> = async () => finish()
  publish()
  if (running) queue.notifyBusy()
  if (state === 'queued x2') {
    queueAdapter.enqueue(append('Check the generated selection diff.', effortControl.state === 'supported' ? effortControl.values.at(-1) : undefined))
    queueAdapter.enqueue(append('Summarize the keyboard navigation findings.', effortControl.state === 'supported' ? effortControl.default : undefined))
  }
  let initialDraft = state === 'typing multi-line' ? 'Review the generated selection change.\nCheck keyboard focus and nested agent tabs.\nKeep the explanation concise.' : state === 'running' ? 'Include the unread counter in the review.' : state === 'offline draft' ? 'Keep this draft while the connection is offline.' : state === 'image attached' ? 'Review the attached generated layout.' : ''
  if (state === 'failed send (restored)') {
    const message = append('Send the generated review summary.')
    try { dispatch(message, false) }
    catch (error) {
      if (!(error instanceof MessageNotSentError)) throw error
      initialDraft = message.content.filter(part => part.type === 'text').map(part => part.text).join('\n')
    }
  }
  return {
    getSnapshot: () => snapshot,
    subscribe: (notify: () => void) => { listeners.add(notify); return () => { listeners.delete(notify) } },
    initialDraft,
    imageFile: state === 'image attached' ? new File([image], 'generated-layout.svg', { type: 'image/svg+xml' }) : undefined,
    finish,
    reconnect: () => { offline = false; failure = undefined; publish() },
  }
}
