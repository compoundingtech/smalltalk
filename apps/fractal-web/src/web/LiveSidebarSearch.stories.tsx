import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { SidebarAgentRow, assistantDarkTheme } from '@smalltalk/fractal-ui/assistant-ui/shell'
import { liveLegacyTheme } from '../ui-compat/live-theme.stylex.ts'
import { AgentFolders } from '../shell/AgentFolders.tsx'
import { SidebarToolbar } from '../shell/sidebar/Toolbar.tsx'
import { defaultFilters } from '../shell/sidebar/state.ts'
import { parseSidebarQuery } from '../shell/sidebar/filter.ts'
import type { SidebarNode } from '../folders/sidebarContract.ts'
import { FolderAttentionSignal } from './AttentionControls.tsx'
import { SidebarEmptyState } from './SidebarEmptyState.tsx'
import { liveSidebarTree } from './liveSidebarTree.ts'
import { sidebarRow } from './sidebarRow.ts'
import { searchAgents } from './sidebarSearch.fixtures.ts'

const needsYou = new Set(['agent/builder'])
const canonicalTree: readonly SidebarNode[] = [{ _tag: 'Folder', id: 'product', label: 'Product', collapsed: false, children: searchAgents.map(agent => ({ _tag: 'Agent', id: agent.ref, subject: agent.ref, label: agent.name })) }]
const deepAgents = [...Array.from({ length: 75 }, (_, index) => ({ ...searchAgents[0]!, ref: `agent/fixture-${index}`, name: `Fixture ${index}` })), ...searchAgents]
const SearchSurface = ({ deep = false }: { readonly deep?: boolean }) => {
  const surface = React.useRef<HTMLElement>(null)
  const [selected, setSelected] = React.useState('')
  const agents = deep ? deepAgents : searchAgents
  const [filters, setFilters] = React.useState(defaultFilters)
  const deferredFilters = React.useDeferredValue(filters)
  const sourceTree = React.useMemo(() => deep ? [{ ...canonicalTree[0]!, children: agents.map(agent => ({ _tag: 'Agent' as const, id: agent.ref, subject: agent.ref, label: agent.name })) }] : canonicalTree, [deep, agents])
  const tree = React.useMemo(() => liveSidebarTree({ tree: sourceTree, agents, needsYou, filters: deferredFilters, stale: false, now: 0 }), [sourceTree, agents, deferredFilters])
  const query = parseSidebarQuery(deferredFilters.query)
  return <section ref={surface} style={{ width: 300, height: 480, display: 'flex', flexDirection: 'column' }} {...stylex.props(assistantDarkTheme, liveLegacyTheme)}>
    <h2>Search agents</h2>
    <SidebarToolbar filters={filters} update={setFilters} hosts={['local']} needsMeCount={needsYou.size} onEnterResults={() => surface.current?.querySelector<HTMLElement>('[role="row"]')?.focus()} />
    {tree.length === 0 ? <SidebarEmptyState query={filters.query} needsMe={filters.needsMe} onClear={() => setFilters(defaultFilters)} /> : null}
    <AgentFolders tree={tree} selectedId={selected} revealKey={filters.query} onSelect={setSelected} canDrop={() => ({ refused: 'Read-only fixture' })} onMove={() => undefined} onCreateFolder={() => undefined} onRenameFolder={() => undefined} onDeleteFolder={() => undefined} onToggleCollapsed={() => undefined} renderFolderSignal={folder => <FolderAttentionSignal folder={folder} refs={needsYou} />} renderAgent={node => { const agent = agents.find(row => row.ref === node.id)!; return <SidebarAgentRow inTree active={selected === node.id} item={{ ...sidebarRow({ agent, stale: false, now: 0 }), needsMe: needsYou.has(agent.ref) }} query={query.text} now={0} extraSignals={['X-needs']} /> }} />
  </section>
}
const assertHighlights = (root: HTMLElement): void => { if (root.querySelectorAll('[data-row-column="title-text"] mark').length === 0) throw new Error('Search hits must be highlighted') }
const meta = { title: 'wf/Sidebar/Live facet search', component: SearchSurface, parameters: { docs: { description: { component: 'The live tree binds shared SidebarToolbar filters, sort and keyboard focus. repo: uses reported checkout intent, state: matches reported lifecycle/activity and tool: matches the reported harness. Unreported facts never match a facet.' } } } } satisfies Meta<typeof SearchSurface>
export default meta
type Story = StoryObj<typeof meta>
export const Search: Story = { play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const field = canvas.getByRole('textbox', { name: 'Search agents' })
  await userEvent.click(canvas.getByRole('heading', { name: 'Search agents' }))
  await userEvent.keyboard('/')
  await expect(field).toHaveFocus()
  const unrelatedRow = canvasElement.querySelectorAll('[data-testid="taste-agent-row"]')[1]!.cloneNode(true)
  await userEvent.type(field, 'builder')
  await waitFor(() => expect(canvasElement.querySelectorAll('[data-testid="taste-agent-row"]')).toHaveLength(1))
  // The reviewer description also says Builder. Restoring that unrelated metadata hit must fail.
  canvasElement.append(unrelatedRow)
  let unrelatedHitRejected = false
  try { await expect(canvasElement.querySelectorAll('[data-testid="taste-agent-row"]')).toHaveLength(1) } catch { unrelatedHitRejected = true }
  await expect(unrelatedHitRejected).toBe(true)
  canvasElement.removeChild(unrelatedRow)
  await userEvent.clear(field); await userEvent.type(field, 'agent/reviewer')
  await waitFor(() => expect(canvasElement.querySelectorAll('[data-testid="taste-agent-row"]')).toHaveLength(1))
  await expect(canvasElement.querySelector('[data-row-column="title-text"]')).toHaveTextContent('reviewer session')
  await userEvent.clear(field)
  await userEvent.type(field, 'repo:product state:working tool:omp builder')
  await waitFor(() => expect(canvasElement.querySelectorAll('[data-testid="taste-agent-row"]')).toHaveLength(1))
  assertHighlights(canvasElement)
  const controls = [...canvasElement.querySelectorAll('[data-row-column="title-text"] mark')].map(mark => ({ mark, text: document.createTextNode(mark.textContent ?? '') }))
  controls.forEach(({ mark, text }) => mark.replaceWith(text))
  try { await expect(() => assertHighlights(canvasElement)).toThrow('Search hits must be highlighted') }
  finally { controls.forEach(({ mark, text }) => text.replaceWith(mark)) }
  await userEvent.clear(field); await userEvent.type(field, 'repo:missing')
  await expect(canvas.getByRole('status')).toHaveTextContent('No agents match “repo:missing”.')
  await userEvent.click(canvas.getByRole('button', { name: 'Clear filters' }))
  await expect(canvasElement.querySelectorAll('[data-testid="taste-agent-row"]')).toHaveLength(3)
  await userEvent.click(canvas.getByRole('button', { name: 'Show finished agents' }))
  await expect(canvasElement.querySelectorAll('[data-testid="taste-agent-row"]')).toHaveLength(2)
  await userEvent.click(canvas.getByRole('button', { name: 'Show finished agents' }))
  await expect(canvasElement.querySelectorAll('[data-testid="taste-agent-row"]')).toHaveLength(3)
} }
export const AllStates: Story = { render: () => <SearchSurface /> }
export const KeyboardSearchReveal: Story = {
  args: { deep: true },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement)
    const field = canvas.getByRole('textbox', { name: 'Search agents' })
    await userEvent.type(field, 'reviewer')
    await userEvent.keyboard('{ArrowDown}')
    const assertEntered = () => {
      if (document.activeElement?.closest('[role="row"]') === null) throw new Error('ArrowDown did not enter the results')
    }
    assertEntered()
    field.focus()
    try { await expect(assertEntered).toThrow('ArrowDown did not enter the results') } finally { await userEvent.keyboard('{ArrowDown}') }
    await userEvent.keyboard('{ArrowDown}{Enter}')
    await waitFor(() => expect(canvasElement.querySelector('[data-wf-agent-ref="agent/reviewer"] [aria-current="page"]')).not.toBeNull())
    await userEvent.click(field)
    await userEvent.keyboard('{Escape}')
    const tree = canvas.getByRole('treegrid', { name: 'Agents and folders' })
    const assertRevealed = () => {
      const active = canvasElement.querySelector<HTMLElement>('[data-wf-agent-ref="agent/reviewer"]')
      if (active === null) throw new Error('Cleared search did not reveal the selected row')
      const row = active.getBoundingClientRect(), viewport = tree.getBoundingClientRect()
      if (row.top < viewport.top || row.bottom > viewport.bottom) throw new Error('Cleared search did not reveal the selected row')
    }
    await waitFor(assertRevealed)
    const scroll = tree.scrollTop
    tree.scrollTop = 0
    const frames = Promise.withResolvers<void>()
    requestAnimationFrame(() => requestAnimationFrame(() => frames.resolve()))
    await frames.promise
    try { await expect(assertRevealed).toThrow('Cleared search did not reveal the selected row') } finally { tree.scrollTop = scroll }
    await waitFor(assertRevealed)
  },
}
export const EmptySearchPosition: Story = {
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement)
    const field = canvas.getByRole('textbox', { name: 'Search agents' })
    await userEvent.type(field, 'no-fictional-agent-matches')
    const notice = canvas.getByRole('status')
    const assertPosition = () => {
      const gap = notice.getBoundingClientRect().top - field.getBoundingClientRect().bottom
      if (gap < 0 || gap > 16) throw new Error('Empty search must sit directly below its field')
    }
    await waitFor(assertPosition)
    notice.style.marginTop = '400px'
    try { await expect(assertPosition).toThrow('Empty search must sit directly below its field') } finally { notice.style.removeProperty('margin-top') }
    assertPosition()
  },
}
