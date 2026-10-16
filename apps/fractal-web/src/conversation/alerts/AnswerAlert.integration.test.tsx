// @vitest-environment jsdom
import { act } from 'react'
import { createRoot } from 'react-dom/client'
import { afterEach, expect, it, vi } from 'vitest'
import type * as Native from '@smalltalk/st3-client/schema'
import { DateTime } from 'effect'

vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}),
  keyframes: () => 'test-animation',
  props: () => ({}),
}))

import { AnswerAlert } from './AnswerAlert.tsx'
import type { AlertAction } from './alert-action-port.ts'

const alert = {
  id: 'attention/ask',
  kind: 'attention',
  revision: 'r1',
  attention_kind: 'person-step',
  title: 'Merge the parser change?',
  detail: '',
  priority: 'high',
  state: 'open',
  requested_at: DateTime.makeUnsafe('2026-10-14T10:00:00Z'),
  person_id: 'person/avery',
  source_id: 'step-run/example/build',
  requester_id: 'agent/example/atlas',
  episode: 'claim/example',
  actions: ['work.done'],
  request: {
    version: 1,
    type: 'decision',
    question: 'Merge the parser change?',
    why_person: 'You own the release.',
    answers: [
      { id: 'merge', label: 'Merge', consequence: 'It joins the merge queue.', outcome: 'accept' },
      { id: 'change', label: 'Change it', consequence: 'The builder revises it.', outcome: 'request_changes' },
    ],
  },
} as unknown as Native.Attention

const button = (container: HTMLElement, label: string) =>
  [...container.querySelectorAll('button')].find((each) => each.textContent === label)

let cleanup: (() => void) | undefined
afterEach(() => cleanup?.())

const mount = async (onAct: (alert: Native.Attention, action: AlertAction) => Promise<{ _tag: 'Sent' }>) => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  cleanup = () => { act(() => root.unmount()); container.remove() }
  await act(async () => root.render(<AnswerAlert alert={alert} onAct={onAct} />))
  return container
}

it('confirms an answer before it sends it, and sends it once', async () => {
  const onAct = vi.fn(async (_alert: Native.Attention, _action: AlertAction) => ({ _tag: 'Sent' as const }))
  const container = await mount(onAct)
  await act(async () => button(container, 'Merge')!.click())
  expect(onAct).not.toHaveBeenCalled()
  expect(container.querySelector('[aria-label="Confirm your answer"]')?.textContent).toContain('Send “Merge” to example/atlas?')
  await act(async () => button(container, 'Cancel')!.click())
  expect(container.querySelector('[aria-label="Confirm your answer"]')).toBeNull()
  await act(async () => button(container, 'Merge')!.click())
  await act(async () => button(container, 'Send')!.click())
  expect(onAct).toHaveBeenCalledTimes(1)
  expect(onAct.mock.calls[0]).toEqual([alert, { type: 'work.done', summary: 'Merge', answer: { id: 'merge' } }])
  expect(container.querySelector('[role="status"]')?.textContent).toBe('Answered. The alert clears once st records it.')
})

it('drops a pending confirmation when the alert is replaced, and keeps the person\'s words', async () => {
  const onAct = vi.fn(async (_alert: Native.Attention, _action: AlertAction) => ({ _tag: 'Sent' as const }))
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  cleanup = () => { act(() => root.unmount()); container.remove() }
  await act(async () => root.render(<AnswerAlert alert={alert} onAct={onAct} />))
  const words = container.querySelector('textarea')!
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(words, 'Only after the fix lands')
    words.dispatchEvent(new Event('input', { bubbles: true }))
  })
  await act(async () => button(container, 'Merge')!.click())
  expect(container.querySelector('[aria-label="Confirm your answer"]')).not.toBeNull()
  // The same alert id comes back at a new revision, carrying a new question.
  const replaced = { ...alert, revision: 'r2', request: { ...(alert as unknown as { request: object }).request, question: 'Merge the reworked parser change?' } } as unknown as Native.Attention
  await act(async () => root.render(<AnswerAlert alert={replaced} onAct={onAct} />))
  expect(container.querySelector('[aria-label="Confirm your answer"]')).toBeNull()
  expect(button(container, 'Send')).toBeUndefined()
  expect(container.querySelector('textarea')!.value).toBe('Only after the fix lands')
  expect(onAct).not.toHaveBeenCalled()
  // A fresh choice is confirmed and sent against the revision now on screen.
  await act(async () => button(container, 'Merge')!.click())
  await act(async () => button(container, 'Send')!.click())
  expect(onAct).toHaveBeenCalledTimes(1)
  expect(onAct.mock.calls[0]![0]).toBe(replaced)
})

it('sends a request for changes only with the person\'s words', async () => {
  const onAct = vi.fn(async (_alert: Native.Attention, _action: AlertAction) => ({ _tag: 'Sent' as const }))
  const container = await mount(onAct)
  expect(button(container, 'Change it')!.disabled).toBe(true)
  const words = container.querySelector('textarea')!
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(words, 'Rename the flag.')
    words.dispatchEvent(new Event('input', { bubbles: true }))
  })
  expect(button(container, 'Change it')!.disabled).toBe(false)
  await act(async () => button(container, 'Change it')!.click())
  await act(async () => button(container, 'Send')!.click())
  expect(onAct.mock.calls[0]![1]).toEqual({ type: 'work.done', summary: 'Rename the flag.', answer: { id: 'change', text: 'Rename the flag.' } })
})
