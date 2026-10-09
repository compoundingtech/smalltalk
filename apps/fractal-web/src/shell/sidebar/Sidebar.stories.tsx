import { useAtom } from '@effect/atom-react'
import { Agent, Attention, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import type { Meta, StoryObj } from '@storybook/react'
import * as stylex from '@stylexjs/stylex'
import { Schema } from 'effect'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import * as React from 'react'

import { scale, tokens } from '../../ui-compat/tokens.stylex.ts'

import { fixtureSource } from '../../data/fixtureSource.ts'
import { DataSourceProvider, useSubjectList } from '../../data/react.tsx'
import { fixtureProjections } from '../../fixtures/projections.ts'
import type { SidebarOperation } from '../../folders/edit.ts'
import { fixtureFolders } from '../../folders/fixture.ts'
import { SubjectAddress } from '../../resources/contract.ts'
import { AgentFolders } from '../AgentFolders.tsx'
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
