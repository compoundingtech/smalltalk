// @vitest-environment jsdom
import * as React from 'react'
import { createRoot } from 'react-dom/client'
import { describe, expect, it, vi } from 'vitest'
import type { AgentFoldersProps, SidebarNode } from '../folders/sidebarContract.ts'

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
Element.prototype.getBoundingClientRect = () => DOMRect.fromRect(viewport)
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
  const row = (label: string): HTMLElement => [...host.querySelectorAll<HTMLElement>('[role="row"]')].find(element => element.textContent?.trim() === label)!
  return { host, row, toggles, dispose: async () => { await React.act(async () => root.unmount()); host.remove() } }
}
describe('bound live folder tree', () => {
  it('derives levels and 16px offsets from RAC hierarchy, including folder members and Unfiled', async () => {
    const view = await mount()
    try {
      for (const [label, level, offset] of [['Alpha', 1, 0], ['Beta', 2, 16], ['Gamma', 3, 32], ['One', 2, 16], ['Two', 2, 16]] as const) {
        const row = view.row(label)
        expect(row, label).toBeDefined()
        expect(row.getAttribute('aria-level'), label).toBe(String(level))
        expect(Number.parseFloat(row.querySelector<HTMLElement>('[data-sidebar-content]')?.style.marginInlineStart ?? ''), label).toBe(offset)
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
})
