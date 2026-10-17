import { useAtom } from '@effect/atom-react'
import { Agent, Attention, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import type { Meta, StoryObj } from '@storybook/react'
import { SidebarAgentRow, assistantDarkTheme, type SidebarAgentRowData } from '@smalltalk/fractal-ui/assistant-ui/shell'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import * as stylex from '@stylexjs/stylex'
import { Schema } from 'effect'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import * as React from 'react'

import { scale, tokens } from '../../ui-compat/tokens.stylex.ts'
import { liveLegacyTheme } from '../../ui-compat/live-theme.stylex.ts'
import { colorVars as c, typeVars as compositionType } from '../../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'

import { fixtureSource } from '../../data/fixtureSource.ts'
import { DataSourceProvider, useSubjectList } from '../../data/react.tsx'
import { fixtureProjections } from '../../fixtures/projections.ts'
import type { SidebarOperation } from '../../folders/edit.ts'
import { fixtureFolders } from '../../folders/fixture.ts'
import { SubjectAddress } from '../../resources/contract.ts'
import { AgentFolders } from '../AgentFolders.tsx'
import type { SidebarNode } from '../../folders/sidebarContract.ts'
import { assertTreeIndentation, measureTreeIndentation } from './treeIndentation.ts'
import { WorkbenchContextProvider } from '../context.tsx'
import { dailyDriver } from '../fixtures/layouts.ts'
import { createWindow, projectWorkspaces, reduceWindow, type WindowAction } from '../workspaces.ts'
import { defaultFilters, fixtureSidebarState, type SidebarFilters } from './state.ts'
import { StatusIcon, statuses, type AgentStatus } from './StatusIcon.tsx'

const styles = stylex.create({
  sidebar: {
    width: '340px',
    maxWidth: '100%',
    minHeight: '100vh',
    backgroundColor: tokens['--ds-background-100'],
    borderInlineEndWidth: '1px',
    borderInlineEndStyle: 'solid',
    borderInlineEndColor: tokens['--ds-gray-alpha-400'],
  },
  indentViewport: { display: 'flex', flexDirection: 'column', height: '100dvh', minHeight: 0, color: c.fg, fontFamily: compositionType.fontSans, fontSize: compositionType.uiSize, lineHeight: compositionType.uiLeading },
  narrow: { width: '280px' },
  statusGrid: { display: 'flex', flexWrap: 'wrap', gap: scale.space4, padding: scale.space4 },
  status: { display: 'flex', alignItems: 'center', gap: scale.space2, fontSize: '0.75rem' },
})
const treeWorld = {
  ...fixtureProjections,
  agents: fixtureProjections.agents.map((agent, index) =>
    decodeUnknownSync(Agent)({
      ...Schema.encodeSync(Agent)(agent),
      ...(index === 3 ? { state: 'running', harness_state: 'idle' } : {}),
      last_activity_at: new Date(fixtureProjections.now - (index + 1) * 180_000).toISOString(),
      current_work:
        index === 0
          ? [
              {
                id: `${agent.id}/sidebar-work`,
                mission_id: fixtureProjections.missions[0]!.id,
                mission_run_id: 'mission-run/fixture-sidebar',
                path: 'implementation/sidebar',
                since: new Date(fixtureProjections.now - 180_000).toISOString(),
                state: 'claimed',
                title: 'Compact sidebar',
                goal: 'Refine folder navigation, compact rows and accessible filtering.',
              },
            ]
          : [],
    }),
  ),
  attention: [
    ...fixtureProjections.attention,
    decodeUnknownSync(Attention)({
      kind: 'attention',
      id: 'attention/sidebar-inbox',
      actions: [],
      attention_kind: 'unread-message',
      detail: 'Review the sidebar restoration.',
      person_id: 'person/operator',
      priority: 'normal',
      requested_at: new Date(fixtureProjections.now).toISOString(),
      revision: '1',
      updated_at: new Date(fixtureProjections.now).toISOString(),
      source_id: fixtureProjections.agents[0]!.id,
      state: 'open',
      title: 'Sidebar review',
    }),
  ],
}
/** Same native arrangement projection as live Fractal; four nesting levels, multiple hosts. */
const folderOps = (refs: readonly string[]): readonly SidebarOperation[] => [
  { op: 'folder.create', id: 'product', name: 'Product', parent: null, key: 'V' },
  { op: 'folder.create', id: 'webfractal', name: 'webfractal', parent: 'product', key: 'V' },
  { op: 'folder.create', id: 'interface', name: 'Interface', parent: 'webfractal', key: 'V' },
  { op: 'folder.create', id: 'review', name: 'Review', parent: 'interface', key: 'V' },
  { op: 'folder.create', id: 'infra', name: 'Infrastructure', parent: null, key: 'W' },
  ...refs.slice(0, 8).map((ref, index): SidebarOperation => ({
    op: 'subject.place', subject: ref,
    folder: ['interface', 'review', 'webfractal', 'product', 'infra', 'infra', 'interface', 'review'][index]!,
    key: String.fromCharCode(86 + index),
  })),
]
const TreeSurface = ({ narrow = false }: { readonly narrow?: boolean }) => {
  const subjects = useSubjectList()
  const [windowAtom] = React.useState(() =>
    Atom.make(createWindow({ seed: dailyDriver, subjects, attention: [] })),
  )
  const [window, setWindow] = useAtom(windowAtom)
  const selection = React.useMemo(
    () => Atom.map(windowAtom, (current) => current.selected),
    [windowAtom],
  )
  const dispatch = (action: WindowAction) =>
    setWindow((current) => reduceWindow({ window: current, action, subjects, attention: [] }))
  const workspaces = projectWorkspaces({ window, subjects, attention: [] })
  return (
    <WorkbenchContextProvider
      value={{
        subjects: new Map(subjects.map((subject) => [subject.ref, subject])),
        focusedRef: null,
        platform: 'other',
        open: (request) =>
          dispatch({
            _tag: 'Layout',
            action: {
              _tag: 'Open',
              input: Schema.decodeSync(SubjectAddress)({
                ref: request.ref,
                presentation: request.presentation ?? 'detail',
              }),
              ...(request.side === undefined ? {} : { side: request.side }),
            },
          }),
      }}
    >
      <div {...stylex.props(styles.sidebar, narrow && styles.narrow)}>
        <AgentFolders workspaces={workspaces} selection={selection} dispatch={dispatch} />
      </div>
    </WorkbenchContextProvider>
  )
}
const FolderTree = ({
  filters = defaultFilters,
  narrow = false,
}: {
  readonly filters?: SidebarFilters
  readonly narrow?: boolean
}) => {
  const [runtime] = React.useState(() => {
    const registry = AtomRegistry.make()
    const source = fixtureSource({ world: treeWorld })
    registry.get(fixtureFolders).edit(folderOps(fixtureProjections.agents.map((agent) => agent.id)))
    registry.set(fixtureSidebarState(source.gateway ?? source.mode).filters, filters)
    return { registry, source }
  })
  return (
    <DataSourceProvider {...runtime}>
      <TreeSurface narrow={narrow} />
    </DataSourceProvider>
  )
}
const meta = {
  title: 'wf/Sidebar/Folder tree',
  component: FolderTree,
  parameters: { layout: 'fullscreen' },
} satisfies Meta<typeof FolderTree>
export default meta
type Story = StoryObj<typeof meta>
export const RealFolderTree: Story = {}
export const NarrowFolderTree: Story = { args: { narrow: true } }
export const NeedsMe: Story = { args: { filters: { ...defaultFilters, needsMe: true } } }
export const FuzzySearch: Story = { args: { filters: { ...defaultFilters, query: 'web' } } }
export const EmptyResults: Story = {
  args: { filters: { ...defaultFilters, query: 'no-such-agent' } },
}
export const StatusPriority: Story = { args: { filters: { ...defaultFilters, sort: 'status' } } }
export const AllStates: Story = {
  render: () => (
    <>
      <div {...stylex.props(styles.statusGrid)}>
        {(Object.keys(statuses) as AgentStatus[]).map((status) => (
          <div key={status} {...stylex.props(styles.status)}>
            <StatusIcon status={status} />
            {statuses[status].label}
          </div>
        ))}
      </div>
      <FolderTree />
    </>
  ),
}
export const EndedOptions: Story = {
  render: () => (
    <div {...stylex.props(styles.statusGrid)}>
      <div {...stylex.props(styles.status)}>
        <StatusIcon status="ended" />
        Stop circle · ended (recommended)
      </div>
      <div {...stylex.props(styles.status)}>
        <StatusIcon status="ended" endedGlyph="flag" />
        Finish flag · ended
      </div>
    </div>
  ),
}

const indentNow = Date.UTC(2026, 9, 8, 12)
const indentRow = (ref: string, title: string): SidebarAgentRowData => ({
  ref, id: ref, title, host: 'local', status: 'idle', statusLabel: 'Idle', freshness: 'live', unread: 2,
  usage: { _tag: 'Known', scope: '24h-root-and-subagents', usd: 4.2, tokens: 2_300_000 },
  duration: { _tag: 'Known', scope: '24h-activity-span', ms: 3_600_000 },
  lastTurn: { _tag: 'Known', kind: 'turn-completed', at: indentNow - 180_000 }, children: [],
})
const detailChild = { ...indentRow('agent/builder/subagent/review/subagent/check/subagent/detail', 'Verify detail'), parentRef: 'agent/builder/subagent/review/subagent/check' }
const checkChild = { ...indentRow('agent/builder/subagent/review/subagent/check', 'Check review'), parentRef: 'agent/builder/subagent/review', children: [detailChild] }
const reviewChild = { ...indentRow('agent/builder/subagent/review', 'Review output'), parentRef: 'agent/builder', children: [checkChild] }
const testChild = { ...indentRow('agent/builder/subagent/test', 'Check tests'), parentRef: 'agent/builder' }
const indentRows = [
  indentRow('agent/root', 'Root session'),
  indentRow('agent/folder', 'Folder session'),
  { ...indentRow('agent/builder', 'Builder session'), children: [reviewChild, testChild] },
]
const indentAgent = (row: SidebarAgentRowData): SidebarNode => ({ _tag: 'Agent', id: row.ref, subject: row.ref, label: row.title })
const indentTree: readonly SidebarNode[] = [
  indentAgent(indentRows[0]!),
  { _tag: 'Folder', id: 'product', label: 'Product', collapsed: false, children: [
    indentAgent(indentRows[1]!),
    { _tag: 'Folder', id: 'interface', label: 'Interface', collapsed: false, children: [indentAgent(indentRows[2]!)] },
  ] },
]

/** Real folder collection plus the production row's recursive, non-RAC subagent expansion. */
const TreeIndentationSurface = () => {
  const [expandedRefs, setExpandedRefs] = React.useState<ReadonlySet<string>>(new Set())
  return <div data-tree-indent-surface="" data-frame-rows="" {...stylex.props(styles.sidebar, styles.indentViewport, liveLegacyTheme, assistantDarkTheme)}>
    <AgentFolders tree={indentTree} canDrop={() => ({ refused: 'Read-only geometry fixture' })}
      onMove={() => undefined} onCreateFolder={() => undefined} onRenameFolder={() => undefined}
      onDeleteFolder={() => undefined} onToggleCollapsed={() => undefined}
      renderAgent={node => <SidebarAgentRow item={indentRows.find(row => row.ref === node.id)!}
        now={indentNow} extraSignals={['X-subs', 'X-unread']} expandedRefs={expandedRefs} active={node.id === 'agent/builder'}
        onToggleChildren={ref => setExpandedRefs(previous => { const next = new Set(previous); if (next.has(ref)) next.delete(ref); else next.add(ref); return next })} />} />
  </div>
}

export const TreeIndentation: Story = {
  render: () => <TreeIndentationSurface />,
  parameters: { docs: { description: { story: 'One 16px step from RAC levels through depths 0–5: folder → folder → agent → subagent → sub-subagent → leaf. The real > n controls expand every agent level. Highlights remain full-width; title and metadata share a text start; timestamps and counts retain aligned columns. The play test forces a zero-step control and requires the same measurement to reject it.' } } },
  play: async ({ canvasElement }) => {
    const surface = canvasElement.querySelector<HTMLElement>('[data-tree-indent-surface]')!
    await document.fonts.ready
    await userEvent.click(await within(surface).findByRole('button', { name: 'Expand 2 subagents' }))
    for (const ref of ['agent/builder/subagent/review', 'agent/builder/subagent/review/subagent/check']) {
      await waitFor(() => expect(surface.querySelector(`[data-wf-agent-ref="${ref}"] [data-testid="taste-agent-row"]`)).not.toBeNull())
      const row = surface.querySelector<HTMLElement>(`[data-wf-agent-ref="${ref}"] [data-testid="taste-agent-row"]`)!
      await userEvent.click(within(row).getByRole('button', { name: 'Expand 1 subagents' }))
    }
    await new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
    const measurement = measureTreeIndentation(surface)
    assertTreeIndentation(measurement)
    surface.dataset.treeIndentMeasurement = JSON.stringify(measurement)
    surface.style.setProperty('--sidebar-tree-step', '0px')
    try {
      const control = measureTreeIndentation(surface)
      surface.dataset.treeIndentControl = JSON.stringify(control)
      await expect(() => assertTreeIndentation(control)).toThrow('expected 16px, received 0px')
    } finally {
      surface.style.removeProperty('--sidebar-tree-step')
    }
  },
}
