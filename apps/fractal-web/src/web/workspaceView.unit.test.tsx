import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { describe, expect, it, vi } from 'vitest'
import { fixtureSource } from '../data/fixtureSource.ts'
import { DataSourceProvider } from '../data/react.tsx'
import { WorkspaceBody } from './LiveAgentWorkspace.tsx'
import { retainPane, workspaceView, type RetainedPane } from './workspaceView.ts'

vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}),
  keyframes: () => 'test-animation',
  props: () => ({}),
}))

describe('workspace body', () => {
  it('renders the terminal as a drawer beside the shown thread', () => {
    const view = workspaceView({ ref: 'terminal/fixture/one' })
    expect(view).toEqual({ _tag: 'Terminal', ref: 'terminal/fixture/one' })
    const source = fixtureSource({ world: { now: 0, agents: [], missions: [], attention: [], events: [], conversations: {}, terminals: {}, envelopes: {}, usage: { _tag: 'undeclared' } } })
    const html = renderToStaticMarkup(createElement(DataSourceProvider, { source, registry: AtomRegistry.make(), children: createElement(WorkspaceBody, { current: 'agent/fixture/one', rosterRefs: ['agent/fixture/one'], view, agentName: 'Fixture', onOpenTool: vi.fn() }) }))
    expect(html).toMatch(/<section aria-label="Terminal drawer" data-testid="terminal-pane">/)
    expect(html).toMatch(/Loading terminal…/)
    expect(html).toMatch(/aria-label="Transcript"/)
    expect(html).toMatch(/^<div><div><div style="display:contents" aria-hidden="false">/)
    expect(html).not.toMatch(/Terminal unavailable/i)
  })
  it('states that any other non-thread view is unavailable', () => {
    const html = renderToStaticMarkup(createElement(WorkspaceBody, { current: 'agent/fixture/one', rosterRefs: ['agent/fixture/one'], view: workspaceView({ ref: 'monitor/quota' }), agentName: 'Fixture', onOpenTool: vi.fn() }))
    expect(html).toMatch(/View unavailable\./)
  })
  it('keeps the thread for the thread tab', () => {
    expect(workspaceView(undefined)).toEqual({ _tag: 'Thread' })
  })
})

describe('retained conversation panes', () => {
  const visit = (refs: readonly string[]) => refs.reduce<readonly RetainedPane[]>((panes, ref) => retainPane({ panes, ref, name: ref, limit: 3 }), [])
  it('keeps opened panes in their first-open order while recency changes', () => {
    expect(visit(['agent/a', 'agent/b', 'agent/c', 'agent/a', 'agent/b']).map(pane => pane.ref)).toEqual(['agent/a', 'agent/b', 'agent/c'])
  })
  it('evicts the least recently opened pane beyond the bound', () => {
    // c was opened before a and b were reopened, so it is the least recent.
    expect(visit(['agent/a', 'agent/b', 'agent/c', 'agent/a', 'agent/b', 'agent/d']).map(pane => pane.ref)).toEqual(['agent/a', 'agent/b', 'agent/d'])
    expect(visit(['agent/a', 'agent/b', 'agent/c', 'agent/d']).map(pane => pane.ref)).toEqual(['agent/b', 'agent/c', 'agent/d'])
  })
  it('renames a retained pane in place', () => {
    const panes = retainPane({ panes: visit(['agent/a', 'agent/b']), ref: 'agent/a', name: 'Renamed', limit: 3 })
    expect(panes.map(pane => [pane.ref, pane.name])).toEqual([['agent/a', 'Renamed'], ['agent/b', 'agent/b']])
  })
})

