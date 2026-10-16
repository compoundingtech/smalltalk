// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type * as Native from '@smalltalk/st3-client/schema'

vi.mock('@stylexjs/stylex', () => import('../../testStylex.ts'))
vi.mock('../../data/react.tsx', () => ({ useAttention: vi.fn(), useDataSource: vi.fn() }))

import { AgentAlerts, waitingCalls } from './AgentAlerts.tsx'

const seat = 'agent/example/atlas'

const prompting = (episode: string, answers: readonly string[]): Native.Attention =>
  ({
    id: 'attention/one',
    kind: 'attention',
    revision: `revision-${episode}`,
    attention_kind: 'harness-prompt',
    title: `${seat} is waiting for you`,
    detail: 'Claude asks you a question',
    priority: 'high',
    state: 'open',
    requested_at: '2026-10-13T10:00:00Z',
    person_id: 'person/avery',
    source_id: seat,
    conversation_id: seat,
    conversation_ids: [seat],
    episode,
    alert: true,
    actions: ['prompt.respond'],
    action_parameters: { 'prompt.respond': { target_id: seat, episode, answers } },
  }) as unknown as Native.Attention

const asking = (callId: string, labels: readonly string[]) => ({
  _tag: 'ToolCall', id: `item/${callId}`, callId, name: 'AskUserQuestion', callSeen: true,
  input: { questions: [{ question: 'Which one?', header: 'Pick', multiSelect: false, options: labels.map((label) => ({ label, description: '' })) }] },
})

let host: HTMLDivElement
let root: Root
beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
})
afterEach(async () => {
  await act(async () => root.unmount())
  host.remove()
  vi.unstubAllGlobals()
})

const button = (label: string) => [...host.querySelectorAll('button')].find((candidate) => candidate.textContent === label)
const press = (label: string) => act(async () => button(label)!.click())

describe('AgentAlerts, pressed', () => {
  it('sends the chosen positions with the episode they were chosen for, and forgets them for a new prompt', async () => {
    const answered: Array<{ episode: string; answer: string; sent: string }> = []
    const onAnswer = async (alert: Native.Attention, answer: string, sent: string) => {
      answered.push({ episode: (alert as unknown as { episode: string }).episode, answer, sent })
      return { _tag: 'Sent' } as const
    }
    const show = (alert: Native.Attention, items: Parameters<typeof waitingCalls>[0]) =>
      act(async () => root.render(<AgentAlerts alerts={[alert]} calls={waitingCalls(items)} onAnswer={onAnswer} />))

    await show(prompting('episode-1', ['answer', 'deny']), [asking('q1', ['Red', 'Blue'])])
    expect(button('Send')!.disabled).toBe(true)
    await press('Blue')
    expect(button('✓ Blue')).toBeDefined()
    await press('Send')
    expect(answered).toEqual([{ episode: 'episode-1', answer: 'answer', sent: '{"0":[1]}' }])
    expect(host.textContent).toContain('Answered.')

    // The same alert now asks a new question: nothing chosen for the old one carries over.
    await show(prompting('episode-2', ['answer', 'deny']), [asking('q2', ['Left', 'Right', 'Up'])])
    expect(button('✓ Blue')).toBeUndefined()
    expect(host.querySelector('[aria-pressed="true"]')).toBeNull()
    expect(button('Send')!.disabled).toBe(true)
    expect(host.textContent).not.toContain('Answered.')
    await press('Up')
    await press('Send')
    expect(answered[1]).toEqual({ episode: 'episode-2', answer: 'answer', sent: '{"0":[2]}' })
  })

  it('allows the one waiting call it shows, by pressing Allow', async () => {
    const answered: string[] = []
    const alert = { ...prompting('episode-3', ['allow', 'deny']), detail: 'Claude asks to use Bash' } as Native.Attention
    await act(async () => root.render(
      <AgentAlerts alerts={[alert]} calls={waitingCalls([{ _tag: 'ToolCall', id: 'item/c1', callId: 'c1', name: 'Bash', input: { command: 'make' }, callSeen: true }])}
        onAnswer={async (_alert, answer, sent) => { answered.push(`${answer}:${sent}`); return { _tag: 'Sent' } }} />,
    ))
    await press('Allow')
    expect(answered).toEqual(['allow:allow'])
  })
})
