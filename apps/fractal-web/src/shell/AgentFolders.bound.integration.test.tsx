// @vitest-environment jsdom
import * as React from 'react'
import { createRoot } from 'react-dom/client'
import { describe, expect, it, vi } from 'vitest'
import type { AgentFoldersProps, SidebarNode } from '../folders/sidebarContract.ts'
import { SidebarAgentRow, sidebarRowAccessibleName } from '@smalltalk/fractal-ui/assistant-ui/shell'
import { sidebarRow } from '../web/sidebarRow.ts'
import { fleetFromAgents } from '../data/projections.ts'
import { Agent, decodeUnknownSync } from '@smalltalk/st3-client/schema'

vi.mock('@stylexjs/stylex', () => ({ create: (styles: Record<string, unknown>) => styles, defineVars: (vars: unknown) => vars, createTheme: () => ({}), keyframes: () => '', props: () => ({}) }))
import { AgentFolders } from './AgentFolders.tsx'

const viewport = { width: 340, height: 800 }
class ViewportObserver implements ResizeObserver {
  constructor(private callback: ResizeObserverCallback) {}
  observe(target: Element): void {
    const size = { inlineSize: 340, blockSize: 800 }
    this.callback([{ target, contentRect: DOMRect.fromRect(viewport), borderBoxSize: [size], contentBoxSize: [size], devicePixelContentBoxSize: [size] }], this)
  }
  unobserve(): void {}
  disconnect(): void {}
}
globalThis.ResizeObserver = ViewportObserver
Element.prototype.getBoundingClientRect = function (this: Element) { return DOMRect.fromRect(this.tagName === 'SPAN' ? {} : viewport) }
Object.defineProperty(Range.prototype, 'getBoundingClientRect', { configurable: true, value: () => new DOMRect() })
Object.defineProperty(Range.prototype, 'getClientRects', { configurable: true, value: () => [] })
vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
vi.stubGlobal('CSS', { escape: (value: string) => value.replaceAll('/', '\\/') })
const fixture: readonly SidebarNode[] = [
  { _tag: 'Folder', id: 'alpha', label: 'Alpha', collapsed: false, children: [
    { _tag: 'Folder', id: 'beta', label: 'Beta', collapsed: false, children: [
      { _tag: 'Folder', id: 'gamma', label: 'Gamma', collapsed: false, children: [] },
    ] },
    { _tag: 'Agent', id: 'agent/one', subject: 'agent/one', label: 'One' },
  ] },
  { _tag: 'Group', id: 'unfiled', label: 'Unfiled', collapsed: false, children: [
    { _tag: 'Agent', id: 'agent/two', subject: 'agent/two', label: 'Two' },
  ] },
]
const mount = async () => {
  const host = document.createElement('div')
  document.body.append(host)
  const root = createRoot(host)
  const toggles: { id: string; collapsed: boolean }[] = []
  const Harness = () => {
    const [collapsed, setCollapsed] = React.useState<ReadonlySet<string>>(new Set())
    const apply = (nodes: readonly SidebarNode[]): readonly SidebarNode[] => nodes.map(node => node._tag === 'Agent' ? node : { ...node, collapsed: collapsed.has(node.id), children: apply(node.children) })
    const props: AgentFoldersProps = {
      tree: apply(fixture), canDrop: () => ({ ok: true }), onMove: () => {}, onCreateFolder: () => {}, onRenameFolder: () => {}, onDeleteFolder: () => {},
      onToggleCollapsed: change => { toggles.push(change); setCollapsed(previous => { const next = new Set(previous); if (change.collapsed) next.add(change.id); else next.delete(change.id); return next }) },
      renderAgent: node => <span>{node.label}</span>,
    }
    return <AgentFolders {...props} />
  }
  await React.act(async () => { root.render(<Harness />) })
  const row = (label: string): HTMLElement => host.querySelector<HTMLElement>(`[role="row"][aria-label="${label}"]`)!
  return { host, row, toggles, dispose: async () => { await React.act(async () => root.unmount()); host.remove() } }
}
describe('bound live folder tree', () => {
  it('derives levels from RAC hierarchy, including folder members and Unfiled', async () => {
    const view = await mount()
    try {
      for (const [label, level] of [['Alpha', 1], ['Beta', 2], ['Gamma', 3], ['One', 2], ['Two', 2]] as const) {
        const row = view.row(label)
        expect(row, label).toBeDefined()
        expect(row.getAttribute('aria-level'), label).toBe(String(level))
      }
    } finally { await view.dispose() }
  })
  it('Right enters an expanded folder; Left moves to its parent, collapses, and Right expands', async () => {
    const view = await mount()
    const key = async (value: string) => { await React.act(async () => { document.activeElement?.dispatchEvent(new KeyboardEvent('keydown', { key: value, bubbles: true })) }) }
    try {
      await React.act(async () => view.row('Alpha').focus())
      await key('ArrowRight')
      expect(document.activeElement).toBe(view.row('Beta'))
      await key('ArrowLeft')
      expect(view.toggles).toContainEqual({ id: 'beta', collapsed: true })
      await key('ArrowLeft')
      expect(document.activeElement).toBe(view.row('Alpha'))
      await key('ArrowLeft')
      expect(view.toggles).toContainEqual({ id: 'alpha', collapsed: true })
      await key('ArrowRight')
      expect(view.toggles).toContainEqual({ id: 'alpha', collapsed: false })
    } finally { await view.dispose() }
  })
  it('retains the native owning row name after RAC repositions memoized content', async () => {
    const host = document.createElement('div')
    document.body.append(host)
    const root = createRoot(host)
    const agent: SidebarNode = { _tag: 'Agent', id: 'agent/atlas', subject: 'agent/atlas', label: 'Atlas Builder' }
    const projected = fleetFromAgents([decodeUnknownSync(Agent)({ kind: 'agent', id: 'agent/atlas', name: 'Atlas Builder', host_id: 'host/host-1', runtime_ids: [], reachability: 'reachable', state: 'running', harness_state: 'idle', blocked_on: null, fault: null, revision: '1', updated_at: '2026-10-04T12:00:00.000Z' })]).agents[0]!
    const item = sidebarRow({ agent: projected, stale: false, now: 5000 })
    const renders = vi.fn()
    const MemoContent = React.memo(() => { renders(); return <SidebarAgentRow inTree item={item} now={5000} /> })
    const content = <MemoContent />
    const props: Omit<AgentFoldersProps, 'tree'> = {
      canDrop: () => ({ ok: true }), onMove: () => {}, onCreateFolder: () => {}, onRenameFolder: () => {}, onDeleteFolder: () => {}, onToggleCollapsed: () => {}, renderAgent: () => content,
      accessibleNames: new Map([[agent.id, sidebarRowAccessibleName(item)]]),
    }
    try {
      await React.act(async () => { root.render(<AgentFolders {...props} tree={[agent]} />) })
      const named = () => [...host.querySelectorAll<HTMLElement>('[role="row"]')].filter(element => element.getAttribute('aria-label') === 'Atlas Builder, idle')
      const row = named()[0]!
      expect(named()).toHaveLength(1)
      expect(renders).toHaveBeenCalledTimes(1)
      await React.act(async () => {
        // RAC may refresh aria props when the same row moves, without rerendering its content.
        row.removeAttribute('aria-labelledby')
        root.render(<AgentFolders {...props} tree={[{ _tag: 'Folder', id: 'before', label: 'Before', collapsed: false, children: [] }, agent]} />)
      })
      await vi.waitFor(() => expect(named()).toEqual([row]))
      expect(renders).toHaveBeenCalledTimes(1)
    } finally {
      await React.act(async () => root.unmount())
      host.remove()
    }
  })
})
