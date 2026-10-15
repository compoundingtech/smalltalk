// @vitest-environment jsdom
import * as React from 'react'
import { createRoot } from 'react-dom/client'
import { describe, expect, it, vi } from 'vitest'
import type { AgentFoldersProps, SidebarNode } from '../folders/sidebarContract.ts'
import { expect as expectA11y, within } from 'storybook/test'
import { SidebarAgentRow, SidebarTreeLevel } from '@smalltalk/fractal-ui/assistant-ui/shell'
import { sidebarRow } from '../web/sidebarRow.ts'
import { sidebarTreeRowName } from '../web/sidebarTreeRowName.ts'
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
const LevelProbe = ({ label }: { readonly label: string }) => <span data-sidebar-level={React.useContext(SidebarTreeLevel)}>{label}</span>
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
      renderAgent: node => <LevelProbe label={node.label} />,
    }
    return <AgentFolders {...props} />
  }
  await React.act(async () => { root.render(<Harness />) })
  const row = (label: string): HTMLElement => within(host).getByRole('row', { name: label })
  return { host, row, toggles, dispose: async () => { await React.act(async () => root.unmount()); host.remove() } }
}
describe('bound live folder tree', () => {
  it('supplies RAC levels to inner content without shifting full-width rows; the browser story proves the 16px step', async () => {
    const view = await mount()
    try {
      for (const [label, level] of [['Alpha', 1], ['Beta', 2], ['Gamma', 3], ['One', 2], ['Two', 2]] as const) {
        const row = view.row(label)
        expect(row, label).toBeDefined()
        expect(row.getAttribute('aria-level'), label).toBe(String(level))
        expect(row.style.getPropertyValue('--tree-item-level'), label).toBe(String(level))
        expect(row.querySelector<HTMLElement>('[data-sidebar-content]')?.style.marginInlineStart, label).toBe('')
        const probe = row.querySelector<HTMLElement>('[data-sidebar-level]')
        if (probe !== null) expect(probe.dataset.sidebarLevel, label).toBe(String(level))
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
  const atlasItem = () => {
    const projected = fleetFromAgents([decodeUnknownSync(Agent)({ kind: 'agent', id: 'agent/atlas', name: 'Atlas Builder', host_id: 'host/host-1', driver: 'codex', runtime_ids: [], reachability: 'reachable', state: 'running', harness_state: 'idle', blocked_on: null, fault: null, revision: '1', updated_at: '2026-10-04T12:00:00.000Z' })]).agents[0]!
    return {
      ...sidebarRow({ agent: projected, stale: false, now: 5000 }),
      context: { _tag: 'Known', usedTokens: 36_000, windowTokens: 200_000, usedPercent: 18, observedAt: 4000 },
      spend: {
        last24h: { _tag: 'Known', usd: 1.25, since: 0, until: 4000, observedAt: 4000 },
        last7d: { _tag: 'Known', usd: 7.5, since: 0, until: 4000, observedAt: 4000 },
      },
    } as const
  }
  const expectFacts = async (row: HTMLElement, title: string, status: RegExp) => {
    await expectA11y(row).toHaveAccessibleName(new RegExp(`^${title}, `))
    await expectA11y(row).toHaveAccessibleName(status)
    await expectA11y(row).toHaveAccessibleName(/main-session context: 18%; 36,000 used tokens; 200,000 token window/)
    await expectA11y(row).toHaveAccessibleName(/24h API-equivalent spend including subagents: \$1\.25/)
    await expectA11y(row).toHaveAccessibleName(/7d API-equivalent spend including subagents: \$7\.5/)
  }
  it('keeps reported facts in the owning row name after RAC repositions memoized content', async () => {
    const host = document.createElement('div')
    document.body.append(host)
    const root = createRoot(host)
    const agent: SidebarNode = { _tag: 'Agent', id: 'agent/atlas', subject: 'agent/atlas', label: 'Atlas Builder' }
    const item = atlasItem()
    const renders = vi.fn()
    const MemoContent = React.memo(() => { renders(); return <SidebarAgentRow inTree item={item} now={5000} layout="SR2-B" extraSignals={['X-model']} /> })
    const content = <MemoContent />
    const props: Omit<AgentFoldersProps, 'tree'> = {
      canDrop: () => ({ ok: true }), onMove: () => {}, onCreateFolder: () => {}, onRenameFolder: () => {}, onDeleteFolder: () => {}, onToggleCollapsed: () => {},
      renderAgent: () => content, agentAccessibleName: () => sidebarTreeRowName(item),
    }
    try {
      await React.act(async () => { root.render(<AgentFolders {...props} tree={[agent]} />) })
      const row = within(host).getByRole('row', { name: /^Atlas Builder, / })
      await expectFacts(row, 'Atlas Builder', /^Atlas Builder, idle\. .*Status: idle;/)
      expect(renders).toHaveBeenCalledTimes(1)
      await React.act(async () => { root.render(<AgentFolders {...props} tree={[{ _tag: 'Folder', id: 'before', label: 'Before', collapsed: false, children: [] }, agent]} />) })
      expect(within(host).getByRole('row', { name: /^Atlas Builder, / })).toBe(row)
      await expectFacts(row, 'Atlas Builder', /^Atlas Builder, idle\. .*Status: idle;/)
      expect(renders).toHaveBeenCalledTimes(1)
    } finally {
      await React.act(async () => root.unmount())
      host.remove()
    }
  })
  describe.each(['root', 'folder', 'unfiled'] as const)('visible agent identity in %s', location => {
    it.each([undefined, 3])('names the owning tree row with its reported facts with unread=%s', async unread => {
      const host = document.createElement('div')
      document.body.append(host)
      const root = createRoot(host)
      const agent: SidebarNode = { _tag: 'Agent', id: 'agent/atlas', subject: 'agent/atlas', label: 'Atlas Builder' }
      const tree: readonly SidebarNode[] = location === 'root' ? [agent] : [{ _tag: location === 'folder' ? 'Folder' : 'Group', id: location, label: location === 'folder' ? 'Workshop' : 'Unfiled', collapsed: false, children: [agent] }]
      const item = { ...atlasItem(), unread }
      const view = (nodes: readonly SidebarNode[], row: typeof item) => <AgentFolders tree={nodes} canDrop={() => ({ ok: true })} onMove={() => {}} onCreateFolder={() => {}} onRenameFolder={() => {}} onDeleteFolder={() => {}} onToggleCollapsed={() => {}} agentAccessibleName={() => sidebarTreeRowName(row)} renderAgent={() => <SidebarAgentRow inTree item={row} now={5000} layout="SR2-B" extraSignals={['X-model']} />} />
      try {
        await React.act(async () => { root.render(view(tree, item)) })
        const row = within(host).getByRole('row', { name: /^Atlas Builder, / })
        await expectFacts(row, 'Atlas Builder', /^Atlas Builder, idle\. .*Status: idle;/)
        await expectA11y(row).not.toHaveAccessibleName(/unknown/i)
        if (unread === undefined) await expectA11y(row).not.toHaveAccessibleName(/unread/i)
        else await expectA11y(row).toHaveAccessibleName(/Unread: 3/)
        const nextAgent = { ...agent, label: 'Orion Builder' }
        const nextTree = tree.map(node => node._tag === 'Agent' ? nextAgent : { ...node, children: [nextAgent] })
        const nextItem = { ...item, title: 'Orion Builder', status: 'working' as const, statusLabel: 'Working', unread: 4 }
        await React.act(async () => { root.render(view(nextTree, nextItem)) })
        const updatedRow = within(host).getByRole('row', { name: /^Orion Builder, / })
        expect(updatedRow).toBe(row)
        await expectFacts(updatedRow, 'Orion Builder', /^Orion Builder, Working\. .*Status: Working;/)
        await expectA11y(updatedRow).toHaveAccessibleName(/Unread: 4/)
      } finally {
        await React.act(async () => root.unmount())
        host.remove()
      }
    })
  })
})
