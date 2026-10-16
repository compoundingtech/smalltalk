// @vitest-environment jsdom
import { act } from 'react'
import { createRoot } from 'react-dom/client'
import { describe, expect, it, vi } from 'vitest'
import type * as Native from '@smalltalk/st3-client/schema'
import { DateTime } from 'effect'

const data = vi.hoisted(() => ({ feed: { _tag: 'Waiting' } as unknown }))
vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}),
  keyframes: () => 'test-animation',
  props: () => ({}),
}))
vi.mock('../data/react.tsx', () => ({ useAttention: () => data.feed }))

import { AlertsPopover, alertsList } from './AlertsPopover.tsx'

const at = Date.parse('2026-10-14T10:00:00Z')
const row = (fields: Record<string, unknown>): Native.Attention =>
  ({
    id: 'attention/ask', kind: 'attention', revision: 'r1', attention_kind: 'person-step', title: 'Ship it?',
    detail: '', priority: 'high', state: 'open', requested_at: DateTime.makeUnsafe(at), person_id: 'person/avery',
    source_id: 'step-run/example/build', requester_id: 'agent/example/atlas', conversation_id: 'agent/example/atlas',
    conversation_ids: ['agent/example/atlas'], alert: true, actions: ['work.done'], ...fields,
  }) as unknown as Native.Attention

const rows = [
  row({ id: 'attention/launch', attention_kind: 'launch-approval', title: 'Launch the nightly mission', requester_id: 'agent/example/planner', conversation_id: 'agent/example/planner', conversation_ids: ['agent/example/planner'] }),
  row({ id: 'attention/ask', requested_at: DateTime.makeUnsafe(at - 3 * 60_000) }),
  row({ id: 'attention/update', alert: false, update: { about: 'run/example' }, title: 'Nightly finished' }),
  row({ id: 'attention/message', attention_kind: 'unread-message', alert: false, title: 'hello' }),
  row({ id: 'attention/done', state: 'resolved', title: 'Old question?' }),
  row({ id: 'attention/gate', attention_kind: 'human-gate', title: 'Review the parser', source_id: 'gate/example', requester_id: undefined, conversation_id: undefined, conversation_ids: undefined }),
]

describe('alertsList', () => {
  it('lists the open alerts, who is stopped first, and the conversation that answers each', () => {
    expect(alertsList(rows, at)).toEqual([
      { id: 'attention/ask', title: 'Ship it?', kind: 'request', from: 'example/atlas', age: '3m', conversation: 'agent/example/atlas' },
      { id: 'attention/gate', title: 'Review the parser', kind: 'review', from: 'gate/example', age: '0s' },
      { id: 'attention/launch', title: 'Launch the nightly mission', kind: 'launch', from: 'example/planner', age: '0s', conversation: 'agent/example/planner' },
    ])
  })
})

describe('AlertsPopover', () => {
  const mount = async (feed: unknown, onOpen = vi.fn()) => {
    vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
    data.feed = feed
    const container = document.createElement('div')
    document.body.append(container)
    const root = createRoot(container)
    await act(async () => root.render(<AlertsPopover onOpen={onOpen} now={at} />))
    return { container, onOpen, unmount: () => { act(() => root.unmount()); container.remove() } }
  }

  it('prints nothing at zero or while alerts load, and never shows unknown as none', async () => {
    for (const feed of [{ _tag: 'Waiting' }, { _tag: 'Observed', value: [], freshness: 'live' }]) {
      const view = await mount(feed)
      expect(view.container.textContent).toBe('')
      view.unmount()
    }
    const failed = await mount({ _tag: 'Unavailable', reason: 'failed', detail: 'socket closed' })
    expect(failed.container.querySelector('button')?.getAttribute('aria-label')).toBe('Alerts could not be loaded')
    failed.unmount()
  })

  it('counts the open alerts and opens the conversation of the one chosen', async () => {
    const view = await mount({ _tag: 'Observed', value: rows, freshness: 'stale' })
    const count = view.container.querySelector('button')!
    expect(count.textContent).toBe('◆ 3 alerts · stale')
    await act(async () => count.click())
    const list = document.querySelector('[aria-label="Alerts"]')!
    const items = [...list.querySelectorAll('button')]
    expect(items.map((item) => item.textContent)).toEqual([
      '◆ Ship it?request · example/atlas · 3m',
      '◆ Review the parserreview · gate/example · 0s',
      '◆ Launch the nightly missionlaunch · example/planner · 0s',
    ])
    // A gate that names no conversation is listed, but there is nothing to open.
    expect(items[1]!.disabled).toBe(true)
    await act(async () => items[0]!.click())
    expect(view.onOpen).toHaveBeenCalledWith('agent/example/atlas')
    expect(document.querySelector('[aria-label="Alerts"]')).toBeNull()
    view.unmount()
  })
})
