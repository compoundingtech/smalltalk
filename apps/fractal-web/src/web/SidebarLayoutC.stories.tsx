import * as React from 'react'
import * as Aria from 'react-aria-components'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { decodeUnknownSync, UsageSummary } from '@smalltalk/st3-client/schema'
import { SidebarAgentRow, assistantDarkTheme, compositionLightTheme } from '@smalltalk/fractal-ui/assistant-ui/shell'
import type { Agent } from '../data/source.ts'
import type { SidebarSpendObservation } from '../extensions/contract.ts'
import type { SidebarNode } from '../folders/sidebarContract.ts'
import { AgentFolders } from '../shell/AgentFolders.tsx'
import { ResponsiveAgentSidebar } from './ResponsiveAgentSidebar.tsx'
import { sidebarRow } from './sidebarRow.ts'
import { liveLegacyTheme } from '../ui-compat/live-theme.stylex.ts'

const now = Date.UTC(2026, 9, 8, 12)
const usage = decodeUnknownSync(UsageSummary)({ aggregation: 'cumulative-per-incarnation-else-response-deltas', incarnation_count: 1, input_tokens: 100, output_tokens: 200, cached_tokens: 300, total_tokens: 600, context: { used_percent: 18, observed_at_unix_ms: now } })
const unknown = { _tag: 'Unknown' } as const
const agents: readonly Agent[] = Array.from({ length: 120 }, (_, index) => ({ ref: `agent/sample-${index}`, terminal: '', name: `Sample ${index}`, lifecycle: unknown, host: 'local', connected: true, harness: 'omp', incarnationId: 'inc/sample', activity: index === 0 ? 'working' : 'idle', status: index === 0 ? 'working' : 'idle', usage: { _tag: 'Known', value: usage }, checkout: unknown, workspace: unknown, startedAt: unknown, endedAt: unknown, blockedOn: unknown, ask: unknown, lastActivityAt: { _tag: 'Known', value: now - 60_000 } }))
const spends = new Map(agents.map((agent, index): [string, SidebarSpendObservation] => [agent.ref, { agentRef: agent.ref, incarnationId: 'inc/sample', nativeSessionId: `00000000-0000-4000-8000-${String(index).padStart(12, '0')}`, spend: { last24h: { _tag: 'Known', usd: 1.84, since: now - 86_400_000, until: now, observedAt: now }, last7d: { _tag: 'Known', usd: 9.25, since: now - 604_800_000, until: now, observedAt: now } } }]))
const agentsByRef = new Map(agents.map(agent => [agent.ref, agent]))
const rows: readonly SidebarNode[] = agents.map(agent => ({ _tag: 'Agent', id: agent.ref, subject: agent.ref, label: agent.name }))
const deepRef = agents.at(-1)!.ref
const LayoutSurface = ({ scheme }: { readonly scheme: 'dark' | 'light' }) => {
  const [width, setWidth] = React.useState(256)
  const [selected, setSelected] = React.useState(agents[0]!.ref)
  const [collapsed, setCollapsed] = React.useState(true)
  const tree: readonly SidebarNode[] = [...rows.slice(0, 100), { _tag: 'Folder', id: 'deep', label: 'Deep work', collapsed, children: rows.slice(100) }]
  return <main data-layout-c-story style={{ width: 1440, height: 700, display: 'flex' }} {...stylex.props(liveLegacyTheme, scheme === 'dark' ? assistantDarkTheme : compositionLightTheme)}>
    <ResponsiveAgentSidebar viewport={1440} width={width} maxWidth={440} collapsed={false} drawerOpen={false} onDrawerOpenChange={() => undefined} onToggleCollapsed={() => undefined} onWidthChange={setWidth} onWidthCommit={setWidth} onWidthReset={() => setWidth(256)}>
      <nav aria-label="Agent roster" style={{ flex: '1 1 0', minHeight: 0, display: 'flex', flexDirection: 'column' }}>
        <AgentFolders tree={tree} selectedId={selected} canDrop={() => ({ refused: 'Read-only fixture' })} onMove={() => undefined} onCreateFolder={() => undefined} onRenameFolder={() => undefined} onDeleteFolder={() => undefined} onToggleCollapsed={({ collapsed }) => setCollapsed(collapsed)} onSelect={setSelected} renderAgent={node => {
          const agent = agentsByRef.get(node.id)!
          return <SidebarAgentRow inTree item={sidebarRow({ agent, stale: false, now, spendObservation: spends.get(agent.ref) })} now={now} variant={width < 300 ? 'SR-1' : 'SR-2'} layout="SR2-C" metricLayout={width < 300 ? 'compact' : 'expanded'} active={selected === agent.ref} extraSignals={[]} onOpen={() => setSelected(agent.ref)} />
        }} />
      </nav>
    </ResponsiveAgentSidebar>
    <section aria-label="Selected thread"><Aria.Button onPress={() => setSelected(deepRef)}>Open deep thread</Aria.Button><p>{selected}</p></section>
  </main>
}
const meta = { title: 'wf/Sidebar/Layout C live binding', component: LayoutSurface, args: { scheme: 'dark' } } satisfies Meta<typeof LayoutSurface>
export default meta
type Story = StoryObj<typeof meta>
const play: NonNullable<Story['play']> = async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const separator = canvas.getByRole('separator', { name: 'Agent sidebar width' })
  separator.focus()
  await expect(separator).toHaveFocus()
  await expect(canvasElement.querySelector('[data-testid="taste-agent-row"]')).toHaveAttribute('data-density', 'SR-1')
  await userEvent.keyboard('{ArrowRight}{ArrowRight}{ArrowRight}{ArrowRight}{ArrowRight}{ArrowRight}{ArrowRight}{ArrowRight}')
  await waitFor(() => expect(separator).toHaveAttribute('aria-valuenow', '320'))
  await expect(canvasElement.querySelector('[data-testid="taste-agent-row"]')).toHaveAttribute('data-density', 'SR-2')
  await expect(canvasElement.querySelector('[data-row-column="metrics"]')).toHaveTextContent('24h $1.8')
  await userEvent.click(canvas.getByRole('button', { name: 'Open deep thread' }))
  const tree = canvas.getByRole('treegrid', { name: 'Agents and folders' })
  const assertRevealed = () => {
    const active = canvasElement.querySelector<HTMLElement>(`[data-wf-agent-ref="${deepRef}"]`)
    if (active === null) throw new Error('Active deep row was not revealed')
    const row = active.getBoundingClientRect(), viewport = tree.getBoundingClientRect()
    if (row.top < viewport.top || row.bottom > viewport.bottom) throw new Error('Active deep row was not revealed')
  }
  await waitFor(assertRevealed)
  await expect(canvasElement.querySelector(`[data-wf-agent-ref="${deepRef}"] [aria-current="page"]`)).not.toBeNull()
  const scroll = tree.scrollTop
  tree.scrollTop = 0
  await new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
  try { await expect(assertRevealed).toThrow('Active deep row was not revealed') } finally { tree.scrollTop = scroll }
  await waitFor(assertRevealed)
}
export const Dark: Story = { play }
export const Light: Story = { args: { scheme: 'light' }, play }
