import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { describe, expect, it, vi } from 'vitest'
import { fixtureSource } from '../data/fixtureSource.ts'
import { DataSourceProvider } from '../data/react.tsx'
import { WorkspaceBody } from './LiveAgentWorkspace.tsx'
import { retainPane, workspaceView, type RetainedPane } from './workspaceView.ts'

vi.mock('@stylexjs/stylex', () => import('../testStylex.ts'))

describe('workspace body', () => {
  it('renders the terminal as a drawer beside the shown thread', () => {
    const view = workspaceView({ ref: 'terminal/fixture/one' })
    expect(view).toEqual({ _tag: 'Terminal', ref: 'terminal/fixture/one' })
    const source = fixtureSource({ world: { now: 0, agents: [], missions: [], attention: [], events: [], conversations: {}, terminals: {}, envelopes: {}, usage: { _tag: 'undeclared' } } })
    const html = renderToStaticMarkup(createElement(DataSourceProvider, { source, registry: AtomRegistry.make(), children: createElement(WorkspaceBody, { current: 'agent/fixture/one', rosterRefs: ['agent/fixture/one'], view, agentName: 'Fixture', onOpenTool: vi.fn() }) }))
    expect(html).toMatch(/<section aria-label="Terminal drawer" data-testid="terminal-pane">/)
    expect(html).toMatch(/data-system-state="loading"[^>]*>.*Loading terminal</)
    expect(html).toMatch(/aria-label="Transcript"/)
    expect(html).toMatch(/^<div><div><div style="display:contents" aria-hidden="false">/)
    expect(html).not.toMatch(/Terminal unavailable/i)
  })
  it('states that any other non-thread view is unavailable', () => {
    const html = renderToStaticMarkup(createElement(WorkspaceBody, { current: 'agent/fixture/one', rosterRefs: ['agent/fixture/one'], view: workspaceView({ ref: 'monitor/quota' }), agentName: 'Fixture', onOpenTool: vi.fn() }))
    expect(html).toMatch(/data-system-state="empty"/)
    expect(html).toMatch(/View not available here\. This web client does not render this view yet</)
  })
  it('opens the Missions screen from its tab', () => {
    expect(workspaceView({ ref: 'missions' })).toEqual({ _tag: 'Missions' })
    const html = renderToStaticMarkup(createElement(WorkspaceBody, { current: '', rosterRefs: [], view: workspaceView({ ref: 'missions' }), agentName: 'Fixture', onOpenTool: vi.fn() }))
    expect(html).toMatch(/data-system-state="loading"[^>]*>.*Loading missions</)
    expect(html).not.toMatch(/Choose an agent/)
  })
  it('opens a document subject, by name or exact reference, as a document view', () => {
    expect(workspaceView({ ref: 'doc/garden-notes' })).toEqual({ _tag: 'Document', ref: 'doc/garden-notes' })
    const html = renderToStaticMarkup(createElement(WorkspaceBody, { current: 'agent/fixture/one', rosterRefs: ['agent/fixture/one'], view: workspaceView({ ref: 'doc/garden-notes' }), agentName: 'Fixture', onOpenTool: vi.fn(), onOpenDocument: vi.fn() }))
    expect(html).toMatch(/data-system-state="loading"[^>]*>.*Loading document</)
    expect(html).not.toMatch(/View not available/)
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

