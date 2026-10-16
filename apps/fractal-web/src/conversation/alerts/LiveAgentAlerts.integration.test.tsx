// @vitest-environment jsdom
import { act } from 'react'
import { createRoot } from 'react-dom/client'
import { expect, it, vi } from 'vitest'
import type * as Native from '@smalltalk/st3-client/schema'
import { DateTime } from 'effect'

const data = vi.hoisted(() => ({ attention: [] as unknown[], act: vi.fn() }))
vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}),
  keyframes: () => 'test-animation',
  props: () => ({}),
}))
vi.mock('../../data/react.tsx', () => ({
  useAttention: () => ({ _tag: 'Observed', value: data.attention, freshness: 'live' }),
  useDataSource: () => ({ alertActions: { act: data.act } }),
}))

import { LiveAgentAlerts } from './AgentAlerts.tsx'
import type { UxTelemetry } from '../../telemetry/ux.ts'

const seat = 'agent/example/atlas'
const ask = (id: string): Native.Attention =>
  ({
    id, kind: 'attention', revision: 'r1', attention_kind: 'person-step', title: `${id}?`, detail: '',
    priority: 'high', state: 'open', requested_at: DateTime.makeUnsafe('2026-10-14T10:00:00Z'),
    person_id: 'person/avery', source_id: `step-run/${id}`, requester_id: seat, conversation_id: seat,
    conversation_ids: [seat], alert: true, actions: ['work.done'],
  }) as unknown as Native.Attention

const click = async (container: HTMLElement, label: string) =>
  act(async () => [...container.querySelectorAll('button')].find((each) => each.textContent === label)!.click())

it('times each answered alert until the strip no longer shows it, and drops refused answers', async () => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  const landed = vi.fn(() => () => {})
  const cancel = vi.fn()
  const ux = { beginAlertAnswer: vi.fn(() => ({ landed, cancel })) } as unknown as UxTelemetry
  data.attention = [ask('attention/one'), ask('attention/two')]
  data.act.mockResolvedValueOnce({ _tag: 'Refused', detail: 'st no longer offers this answer here.' }).mockResolvedValueOnce({ _tag: 'Sent' })
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  const render = () => act(async () => root.render(<LiveAgentAlerts agentRef={seat} items={[]} ux={ux} />))
  await render()
  expect(container.textContent).toContain('◆ 2 alerts')

  await click(container, 'Yes')
  await click(container, 'Send')
  expect(container.querySelector('[role="status"]')?.textContent).toBe('st no longer offers this answer here.')
  expect(cancel).toHaveBeenCalledOnce()

  await click(container, 'Yes')
  await click(container, 'Send')
  expect(data.act).toHaveBeenLastCalledWith({ alert: data.attention[0], action: { type: 'work.done', summary: 'Yes' } })
  expect(landed).not.toHaveBeenCalled()

  // st records the answer: the alert leaves the attention collection and the strip.
  data.attention = [ask('attention/two')]
  await render()
  expect(landed).toHaveBeenCalledOnce()
  expect(container.textContent).not.toContain('attention/one?')
  expect(ux.beginAlertAnswer).toHaveBeenCalledTimes(2)
  act(() => root.unmount())
  container.remove()
})
