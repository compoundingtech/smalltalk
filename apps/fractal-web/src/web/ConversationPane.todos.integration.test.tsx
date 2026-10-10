// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot } from 'react-dom/client'
import { expect, it, vi } from 'vitest'

const state = vi.hoisted(() => ({ tag: 'Waiting' }))
vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}),
  keyframes: () => 'test-animation',
  props: () => ({}),
}))
vi.mock('@effect/atom-react', () => ({ useAtomValue: () => false }))
vi.mock('../data/react.tsx', () => ({
  useDataSource: () => ({}), useConversation: () => state.tag === 'Waiting' ? { _tag: 'Waiting' } : { _tag: 'Unavailable', reason: 'failed', detail: 'example' },
  useConversationSync: () => undefined, useFeedInterest: () => {}, useGrants: () => ({ messageSend: 'ungranted' }), useNow: () => 0,
}))
vi.mock('./composerSend.ts', () => ({ composerSendBinding: () => ({ runtime: {}, disabledReason: 'Waiting for access' }) }))
vi.mock('../../../../packages/fractal-ui/src/assistant-ui/EmbraceRuntime.tsx', () => ({ EmbraceRuntimeProvider: ({ children }: { children: React.ReactNode }) => children }))
vi.mock('../../../../packages/fractal-ui/src/assistant-ui/composition/Transcript.tsx', () => ({ Transcript: () => <section>Transcript</section> }))
vi.mock('../../../../packages/fractal-ui/src/assistant-ui/EmbraceComposer.tsx', () => ({ EmbraceComposer: () => <textarea aria-label="Message" /> }))
vi.mock('../conversation/todos/AgentTodos.tsx', () => ({ LiveAgentTodos: () => <details aria-label="Harness todos"><summary>Todos · Example phase 1/2</summary></details> }))
import { ConversationPane } from './ConversationPane.tsx'

it('keeps Todos out of the pane and preserves the composer during conversation loading and unavailability', async () => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  try {
    await act(async () => root.render(<ConversationPane agentRef="agent/example" agentName="Example" onOpenTool={() => {}} />))
    const composer = container.querySelector('textarea')
    expect(container.querySelector('[aria-label="Harness todos"]')).toBeNull()
    expect(composer).not.toBeNull()
    state.tag = 'Unavailable'
    await act(async () => root.render(<ConversationPane agentRef="agent/example" agentName="Example" onOpenTool={() => {}} />))
    expect(container.querySelector('[aria-label="Harness todos"]')).toBeNull()
    expect(container.querySelector('textarea')).toBe(composer)
  } finally {
    await act(async () => root.unmount())
    container.remove()
    vi.unstubAllGlobals()
  }
})
