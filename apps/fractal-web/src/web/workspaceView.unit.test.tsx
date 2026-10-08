import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it, vi } from 'vitest'
import { ChangesPanelNotice, WorkspaceBody } from './LiveAgentWorkspace.tsx'
import { changesNotice, liveChangesState, workspaceView } from './workspaceView.ts'

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

describe('changes panel', () => {
  it('tells loading, no observed changes and unavailable apart', () => {
    expect(changesNotice({ _tag: 'Loading' })).toBe('Loading changes…')
    expect(changesNotice({ _tag: 'NoObservedChanges' })).toBe('No changes observed for this thread.')
    expect(changesNotice({ _tag: 'Unavailable', reason: 'Reported reason.' })).toBe('Changes unavailable. Reported reason.')
  })
  it('renders the live panel as unavailable, never as pending on transcript observations', () => {
    const html = renderToStaticMarkup(createElement(ChangesPanelNotice, { state: liveChangesState }))
    expect(html).toMatch(/Changes unavailable\. This web client does not read change data yet\./)
    expect(html).not.toMatch(/verified transcript observations/)
  })
})
