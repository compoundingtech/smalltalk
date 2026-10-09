import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it, vi } from 'vitest'
import { WorkspaceBody } from './LiveAgentWorkspace.tsx'
import { workspaceView } from './workspaceView.ts'

vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}),
  keyframes: () => 'test-animation',
  props: () => ({}),
}))

describe('workspace body', () => {
  it('shows a terminal-specific unavailable state instead of the transcript when a terminal pane is selected', () => {
    const view = workspaceView({ ref: 'terminal/fixture/one' })
    expect(view._tag).toBe('TerminalUnavailable')
    const html = renderToStaticMarkup(createElement(WorkspaceBody, { current: 'agent/fixture/one', view, agentName: 'Fixture', onOpenTool: vi.fn() }))
    expect(html).toMatch(/Terminal unavailable\. This web client has no terminal renderer yet\./)
    expect(html).not.toMatch(/conversation|transcript/i)
  })
  it('states that any other non-thread view is unavailable', () => {
    const html = renderToStaticMarkup(createElement(WorkspaceBody, { current: 'agent/fixture/one', view: workspaceView({ ref: 'monitor/quota' }), agentName: 'Fixture', onOpenTool: vi.fn() }))
    expect(html).toMatch(/View unavailable\./)
  })
  it('keeps the thread for the thread tab', () => {
    expect(workspaceView(undefined)).toEqual({ _tag: 'Thread' })
  })
})

