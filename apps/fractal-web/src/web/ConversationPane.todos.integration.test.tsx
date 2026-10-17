// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot } from 'react-dom/client'
import { expect, it, vi } from 'vitest'

const state = vi.hoisted(() => ({ tag: 'Waiting' }))
vi.mock('@stylexjs/stylex', () => import('../testStylex.ts'))
vi.mock('@effect/atom-react', () => ({ useAtomValue: () => false }))
vi.mock('../data/react.tsx', () => ({
  useDataSource: () => ({}), useConversation: () => state.tag === 'Waiting' ? { _tag: 'Waiting' } : { _tag: 'Unavailable', reason: 'failed', detail: 'example' },
  useConversationSync: () => undefined, useFeedInterest: () => {}, useGrants: () => ({ messageSend: 'ungranted' }), useObservedNow: () => ({ _tag: 'Live', now: 0 }), useAttention: () => ({ _tag: 'Waiting' }),
}))
vi.mock('../conversation/todos/AgentTodos.tsx', () => ({ LiveAgentTodos: () => <details aria-label="Harness todos"><summary>Todos · Example phase 1/2</summary></details> }))
import { ConversationPane } from './ConversationPane.tsx'

it('keeps Todos out of the pane and preserves the composer during conversation loading and unavailability', async () => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  // jsdom has no layout; retain the real kit runtime, composer and transcript
  // while leaving their viewport observers inert in this identity-only test.
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
  vi.stubGlobal('requestAnimationFrame', () => 1)
  vi.stubGlobal('cancelAnimationFrame', () => {})
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  try {
    await act(async () => root.render(<ConversationPane agentRef="agent/example" agentName="Example" onOpenTool={() => {}} />))
    const composer = container.querySelector('textarea')
    expect(container.querySelector('[aria-label="Harness todos"]')).toBeNull()
    expect(composer).not.toBeNull()
    expect(container.querySelector('[aria-label="Loading conversation"]')).not.toBeNull()
    state.tag = 'Unavailable'
    await act(async () => root.render(<ConversationPane agentRef="agent/example" agentName="Example" onOpenTool={() => {}} />))
    expect(container.querySelector('[aria-label="Harness todos"]')).toBeNull()
    expect(container.querySelector('textarea')).toBe(composer)
    expect(container.querySelector('[data-testid="transcript-unavailable"]')).not.toBeNull()
    expect(container.querySelector('[aria-label="Loading conversation"]')).toBeNull()
  } finally {
    await act(async () => root.unmount())
    container.remove()
    vi.unstubAllGlobals()
  }
})
