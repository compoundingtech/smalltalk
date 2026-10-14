import { useAtom, useAtomValue } from '@effect/atom-react'
import * as stylex from '@stylexjs/stylex'
import { Schema } from 'effect'
import type * as Atom from 'effect/reactivity/Atom'
import * as React from 'react'
import { SidebarTree, type SidebarTreeHandle, type SidebarTreeItem, type SidebarTreeProps } from '@smalltalk/fractal-ui/assistant-ui/shell'
import {
  Button as AriaButton,
  useDragAndDrop,
  type Key,
  type DragAndDropOptions,
  type TreeProps,
} from 'react-aria-components'

import { Button } from '../ui-compat/components.tsx'
import { scale, tokens } from '../ui-compat/tokens.stylex.ts'
import { RenderProfiler } from '../telemetry/meters.tsx'

import { matchPositions } from '../command-palette/fuzzy.ts'
import {
  useAgent,
  useDataSource,
  useFleetSelection,
  useNow,
  useSubjectSelection,
} from '../data/react.tsx'
import type { Feed, Fleet } from '../data/source.ts'
import { folders } from '../folders/client.ts'
import { liveAncestor, project, type ProjectedFolder } from '../folders/core.mts'
import { refusalText } from '../folders/edit.ts'
import { fixtureFolders } from '../folders/fixture.ts'
import { WfIcon } from '../icons/WfIcon.tsx'
import { SubjectAddress } from '../resources/contract.ts'
import { AgentUsageRow, AgentUsageSignals, openAgentUsage } from './AgentUsage.tsx'
import { filterFolders, matchesRow, parseSidebarQuery, sortMembers, type SidebarRow } from './sidebar/filter.ts'
import { fixtureSidebarState, sidebarState } from './sidebar/state.ts'
import type { SidebarFilters } from './sidebar/state.ts'
import { StatusIcon, agentStatus, compactTime, type AgentStatus } from './sidebar/StatusIcon.tsx'
import { SidebarToolbar } from './sidebar/Toolbar.tsx'
import type { Workspace, WindowAction } from './workspaces.ts'
import type { AgentFoldersProps, SidebarNode, SidebarMoveTarget } from '../folders/sidebarContract.ts'

const unfiled = 'wf/unfiled'
const styles = stylex.create({
  heading: {
    display: 'flex',
    alignItems: 'center',
    justifyContent: 'space-between',
    paddingBlock: scale.space1,
    paddingInline: scale.space2,
    minHeight: '28px',
    fontWeight: 600,
  },
  // The parent assigns available flex space; the kit owns the only scrolling tree.
  treeViewport: { display: 'flex', flexDirection: 'column', flex: '1 1 0', minHeight: 0, overflow: 'hidden' },
  row: {
    display: 'flex',
    flexDirection: 'column',
    position: 'relative',
    gap: '3px',
    padding: '5px 6px',
    marginBlock: '1px',
    borderRadius: scale.radiusSm,
    backgroundColor: {
      default: 'transparent',
      ':hover': tokens['--ds-gray-alpha-100'],
      ':focus-within': tokens['--ds-gray-alpha-100'],
    },
  },
  rail: {
    borderInlineStartWidth: '2px',
    borderInlineStartStyle: 'solid',
    borderInlineStartColor: tokens['--ds-gray-700'],
  },
  workingRail: { borderInlineStartColor: tokens['--ds-green-900'] },
  waitingRail: { borderInlineStartColor: tokens['--ds-amber-900'] },
  staleRail: { borderInlineStartColor: tokens['--ds-red-900'] },
  secondLine: {
    display: 'flex',
    alignItems: 'center',
    flexWrap: 'wrap',
    gap: '6px',
    minWidth: 0,
    minHeight: '14px',
  },
  selected: { backgroundColor: tokens['--ds-gray-alpha-200'] },
  rowContent: { width: '100%' },
  line: { display: 'flex', alignItems: 'center', gap: '6px', minWidth: 0 },
  nameSpace: { display: 'flex', minWidth: 0, flexGrow: 1 },
  nameSlot: { display: 'flex', alignItems: 'center', minWidth: 0 },
  name: {
    minWidth: 0,
    overflow: 'hidden',
    textOverflow: 'ellipsis',
    whiteSpace: 'nowrap',
    fontSize: '0.75rem',
    fontWeight: 500,
  },
  description: {
    display: 'block',
    flex: '1 1 0',
    minWidth: 0,
    overflow: 'hidden',
    textOverflow: 'ellipsis',
    whiteSpace: 'nowrap',
    color: tokens['--ds-gray-900'],
    fontSize: '0.6875rem',
  },
  meta: {
    display: 'flex',
    alignItems: 'center',
    gap: '5px',
    fontSize: '0.625rem',
    fontVariantNumeric: 'tabular-nums',
    color: tokens['--ds-gray-900'],
    flexShrink: 0,
  },
  signals: { flexWrap: 'wrap', flexShrink: 1, minWidth: 0 },
  count: {
    fontSize: '0.6875rem',
    fontVariantNumeric: 'tabular-nums',
    color: tokens['--ds-gray-900'],
  },
  host: {
    borderRadius: scale.radiusSm,
    paddingInline: '3px',
    backgroundColor: tokens['--ds-gray-alpha-100'],
    whiteSpace: 'nowrap',
  },
  folder: {
    display: 'flex',
    alignItems: 'center',
    position: 'relative',
    gap: '4px',
    padding: '4px 8px',
    minHeight: '30px',
  },
  // React Aria Components 1.21.1 Tree hardcodes hasDragButton: true and requires
  // Button slot="drag" for keyboard/screen-reader DnD. Pointer drag is on the
  // TreeItem itself; the text Move action reveals only on focus, never on hover.
  folderLabel: {
    minWidth: 0,
    flexGrow: 1,
    display: 'flex',
    alignItems: 'center',
    gap: '5px',
    fontSize: '0.75rem',
    fontWeight: 500,
  },
  folderCount: {
    minWidth: '18px',
    flexShrink: 0,
    textAlign: 'right',
    color: tokens['--ds-gray-900'],
    fontSize: '0.6875rem',
    fontVariantNumeric: 'tabular-nums',
  },
  notice: { color: tokens['--ds-gray-900'], fontSize: '0.6875rem', padding: scale.space2 },
  highlight: {
    color: tokens['--ds-gray-1000'],
    backgroundColor: tokens['--ds-gray-alpha-200'],
    fontWeight: 600,
  },
  drag: { position: 'absolute', insetInlineEnd: 0, top: 0, opacity: { default: 0, ':focus-visible': 1 }, width: 16, height: 16 },
  treeContent: { position: 'relative', width: '100%' },
})

/** Fractal folders own membership; view-only filters, order and collapse use shared persistence. */
const LegacyAgentFolders = React.memo(
  ({
    workspaces,
    selection,
    dispatch,
  }: {
    readonly workspaces: readonly Workspace[]
    readonly selection: Atom.Atom<string>
    readonly dispatch: (action: WindowAction) => void
  }) => {
    const source = useDataSource()
    const snapshot = useAtomValue(source.mode === 'fixtures' ? fixtureFolders : folders)
    const editRefusal = snapshot.refusal
    const stateAtoms = (source.mode === 'fixtures' ? fixtureSidebarState : sidebarState)(
      source.gateway ?? source.mode,
    )
    const [filters, setFilters] = useAtom(stateAtoms.filters)
    const [collapsedIds, setCollapsedIds] = useAtom(stateAtoms.collapsed)
    // Collection membership/order depends only on active filters. Presentation updates belong
    // to the keyed row below; feeding them through Tree rebuilds React Aria's entire collection.
    const equalFleet = React.useCallback(
      (left: Feed<Fleet>, right: Feed<Fleet>) => sameFolderFleet(left, right, filters),
      [filters],
    )
    const fleet = useFleetSelection({ select: identityFleet, equal: equalFleet })
    const selectAttention = React.useCallback(
      (subjects: readonly { readonly ref: string; readonly attention?: boolean }[]) =>
        filters.needsMe || filters.sort === 'status'
          ? subjects
              .filter((subject) => subject.ref.startsWith('agent/') && subject.attention === true)
              .map((subject) => subject.ref)
          : noAttention,
      [filters.needsMe, filters.sort],
    )
    const attention = useSubjectSelection({ select: selectAttention, equal: sameRefs })
    const needsYou = new Set(attention)
    const doc = snapshot.doc
    const view = project(
      doc,
      workspaces.map((workspace) => workspace.id),
      { rootOrder: 'placement' },
    )
    const folderViews = new Map<string, ProjectedFolder>()
    const parents = new Map<string, string | null>()
    const indexFolders = ({
      items,
      parent,
    }: {
      readonly items: readonly ProjectedFolder[]
      readonly parent: string | null
    }) => {
      for (const folder of items) {
        folderViews.set(folder.id, folder)
        parents.set(folder.id, parent)
        indexFolders({ items: folder.folders, parent: folder.id })
      }
    }
    indexFolders({ items: view.folders, parent: null })
    const agents = new Map(
      fleet._tag === 'Observed'
        ? fleet.value.agents.map((agent) => [agent.ref, agent] as const)
        : [],
    )
    const rows = new Map<string, SidebarRow>(
      workspaces.map((workspace) => {
        const agent = agents.get(workspace.id)
        const status = agentStatus({ agent, fleet })
        return [
          workspace.id,
          {
            workspace,
            agent,
            status,
            needsMe: status === 'waiting' || needsYou.has(workspace.id),
          },
        ]
      }),
    )
    const parsedQuery = parseSidebarQuery(filters.query)
    const visibleFolders = filterFolders({
      folders: view.folders,
      rows: rows,
      filters: filters,
      sort: filters.sort,
      query: parsedQuery,
    })
    const visibleUnfiled = sortMembers({
      members: view.unfiled.filter((id) =>
        matchesRow({ row: rows.get(id)!, filters: filters, query: parsedQuery }),
      ),
      rows: rows,
      sort: filters.sort,
    })
    const searching = parsedQuery.text !== ''
    // Any typed query term, including facet-only and machine-only searches, reveals matches and
    // keeps temporary expansion out of the saved layout; highlights still use free text alone.
    const querying = searching || parsedQuery.facets.length > 0 || parsedQuery.machines.length > 0
    const highlight = (text: string) => {
      if (!searching) return text
      const positions = matchPositions({ query: parsedQuery.text, text })
      let sourceOffset = 0
      return [...text].map((character, index) => {
        const start = sourceOffset
        sourceOffset += character.length
        return positions.has(index) ? (
          <mark key={start} {...stylex.props(styles.highlight)}>
            {character}
          </mark>
        ) : (
          character
        )
      })
    }
    const multipleHosts = new Set(workspaces.map((workspace) => workspace.host)).size > 1
    const treeRef = React.useRef<SidebarTreeHandle>(null)
    // Collection caches are independent of Workspace object identity. A changed member rebuilds
    // only its ancestor branches, retaining every sibling's item and folder content node.
    const folderElements = React.useMemo(
      // Cache-busting dependencies are load-bearing: cached elements close over the current document.
      () => new Map<string, CachedFolderElement>(),
      // eslint-disable-next-line react-hooks/exhaustive-deps -- folder-command closures invalidate with these.
      [doc, selection, dispatch, filters.query, multipleHosts, source],
    )
    // Local layouts and notification objects are not row presentation. Cache by the exact
    const agentElements = React.useMemo(
      () => new Map<string, CachedAgentElement>(),
      // eslint-disable-next-line react-hooks/exhaustive-deps -- row-action closures invalidate with these.
      [selection, dispatch, filters.query, multipleHosts, source],
    )
    for (const id of agentElements.keys()) if (!rows.has(id)) agentElements.delete(id)
    const renderAgent = ({ id, depth }: { readonly id: string; readonly depth: number }) => {
      const { workspace } = rows.get(id)!
      const cached = agentElements.get(id)
      if (
        cached !== undefined &&
        cached.element.depth === depth &&
        cached.workspace.title === workspace.title &&
        cached.workspace.host === workspace.host &&
        cached.workspace.unread.length === workspace.unread.length
      )
        return cached.element
      // Do not retain an obsolete pane layout through a long-lived collection element.
      const rowWorkspace: AgentRowWorkspace = {
        id,
        title: workspace.title,
        host: workspace.host,
        unread: workspace.unread,
      }
      const element: AgentFolderTreeItem = {
        id, depth, textValue: workspace.title, agentRef: id,
        onAction: () => {
          dispatch({ _tag: 'SelectWorkspace', id })
          dispatch({
            _tag: 'Layout', workspace: id,
            action: { _tag: 'Open', input: Schema.decodeSync(SubjectAddress)({ ref: id, presentation: 'detail' }) },
          })
        },
        onHover: () => source.prefetchConversation?.(id),
        render: () => <AgentFolderRow workspace={rowWorkspace} selection={selection} dispatch={dispatch} query={parsedQuery.text} multipleHosts={multipleHosts} />,
      }
      agentElements.set(id, { workspace: rowWorkspace, element })
      return element
    }
    const renderFolder = ({
      folder, depth,
    }: {
      readonly folder: ProjectedFolder
      readonly depth: number
    }): AgentFolderTreeItem => {
      const children = [
        ...folder.folders.map((child) => renderFolder({ folder: child, depth: depth + 1 })),
        ...folder.members.map((id) => renderAgent({ id, depth: depth + 1 })),
      ]
      const count = descendantCount(folder)
      const cached = folderElements.get(folder.id)
      const sameContent =
        cached !== undefined &&
        cached.element.depth === depth &&
        cached.name === folder.name &&
        cached.count === count
      if (sameContent && sameElements(cached.children, children)) return cached.element
      const content: AgentFolderTreeItem['render'] = sameContent ? cached.content : ({ expanded }) => (
        <div {...stylex.props(styles.folder)}>
          <span {...stylex.props(styles.folderLabel)}>
            <WfIcon name={expanded ? 'folderOpen' : 'folder'} />
            <span {...stylex.props(styles.nameSlot)}>
              <span title={folder.name} {...stylex.props(styles.name)}>{highlight(folder.name)}</span>
            </span>
          </span>
          <span {...stylex.props(styles.folderCount)}>{count}</span>
          <ArrangementEditFeedback target={folder.id} />
        </div>
      )
      const element: AgentFolderTreeItem = { id: folder.id, depth, textValue: folder.name, render: content, children }
      folderElements.set(folder.id, { name: folder.name, count, content, children, element })
      return element
    }
    const unfiledChildren = visibleUnfiled.map((id) => renderAgent({ id, depth: 1 }))
    const cachedUnfiled = folderElements.get(unfiled)
    const unfiledContent: AgentFolderTreeItem['render'] =
      cachedUnfiled?.count === visibleUnfiled.length ? cachedUnfiled.content : () => (
        <div {...stylex.props(styles.folder)}>
          <span {...stylex.props(styles.folderLabel)}>Unfiled</span>
          <span {...stylex.props(styles.folderCount)}>{visibleUnfiled.length}</span>
        </div>
      )
    const unfiledElement: AgentFolderTreeItem =
      cachedUnfiled?.content === unfiledContent && sameElements(cachedUnfiled.children, unfiledChildren)
        ? cachedUnfiled.element
        : { id: unfiled, depth: 0, textValue: 'Unfiled', render: unfiledContent, children: unfiledChildren }
    folderElements.set(unfiled, {
      name: 'Unfiled',
      count: visibleUnfiled.length,
      content: unfiledContent,
      children: unfiledChildren,
      element: unfiledElement,
    })
    const collectionCache = React.useMemo(
      (): FolderTreeCollectionCache => ({ items: [], folderIds: [] }),
      // eslint-disable-next-line react-hooks/exhaustive-deps -- rebuild when the folder cache is replaced.
      [folderElements],
    )
    const nextItems = [
      ...visibleFolders.map((folder) => renderFolder({ folder, depth: 0 })),
      ...(visibleUnfiled.length === 0 && (visibleFolders.length > 0 || querying)
        ? []
        : [unfiledElement]),
    ]
    if (
      !sameElements(collectionCache.items, nextItems) ||
      collectionCache.onExpandedChange === undefined
    ) {
      collectionCache.items = nextItems
      collectionCache.folderIds = [...folderViews.keys(), unfiled]
      collectionCache.onExpandedChange = (keys) => {
        if (!querying) setCollapsedIds(collectionCache.folderIds.filter((id) => !keys.has(id)))
      }
    }
    const expanded = React.useMemo(() => {
      const collapsed = new Set(collapsedIds)
      return new Set(collectionCache.folderIds.filter((id) => querying || !collapsed.has(id)))
    }, [collectionCache.folderIds, collapsedIds, querying])
    // Dynamic collections key their renderer cache by item identity; the renderer and its
    // dependencies must not capture live roster values. textValue still changes with the label,
    // including for an offscreen row, so React Aria's keyboard typeahead uses the current name.
    // React Aria owns row focus; capture its DOM event instead of an unsupported TreeItem prop.
    return (
      <section
        aria-label="Agents"
        {...stylex.props(styles.treeViewport)}
        onFocusCapture={(event) => {
          const row = event.target.closest<HTMLElement>('[data-wf-agent-ref]') ?? event.target.closest('[role="row"]')?.querySelector<HTMLElement>('[data-wf-agent-ref]')
          const agentRef = row?.dataset.wfAgentRef
          if (agentRef !== undefined) source.prefetchConversation?.(agentRef)
        }}
      >
        <div {...stylex.props(styles.heading)}>
          <span>
            Agents <span {...stylex.props(styles.count)}>{workspaces.length}</span>
          </span>
        </div>
        <SidebarToolbar
          filters={filters}
          update={setFilters}
          hosts={[...new Set(workspaces.map((workspace) => workspace.host))].toSorted()}
        />
        {(snapshot.sidebarCandidates?.length ?? 0) > 1 ? (
          <div role="note" aria-label="Multiple legacy Sidebars" {...stylex.props(styles.notice)}>
            <p>Multiple legacy Sidebars were found. The lowest UUID is used unless the reserved Sidebar is present.</p>
            <ul>
              {snapshot.sidebarCandidates?.map((candidate) => (
                <li key={candidate.id}>
                  {candidate.label}{candidate.id === snapshot.sidebarSubject ? ' · Currently used' : ''}
                </li>
              ))}
            </ul>
            {snapshot.sidebarSubject === undefined ? null : <p>Currently used Sidebar: {snapshot.sidebarSubject}</p>}
          </div>
        ) : null}
        {snapshot.restoreUnavailable ? (
          <div role="status" aria-label="Removed Sidebar" {...stylex.props(styles.notice)}>
            <p>Sidebar removed.</p>
            <p>Restoring a removed Sidebar is not available yet.</p>
            <Button size="sm" variant="secondary" isDisabled>Restore</Button>
          </div>
        ) : null}
        <AgentFolderTree
          items={collectionCache.items}
          expanded={expanded}
          onExpandedChange={collectionCache.onExpandedChange}
          treeRef={treeRef}
        />
        {editRefusal?.attemptedTargets?.map((target) => (
          <div
            key={target.id}
            data-wf-refused-folder={target.kind === 'folder' ? target.id : undefined}
            data-wf-refused-agent={target.kind === 'agent' ? target.id : undefined}
            data-wf-refusal-reason={editRefusal.reason._tag}
            data-wf-refusal-code={editRefusal.reason.code}
            role="status"
            {...stylex.props(styles.notice)}
          >
            <strong>{target.kind === 'agent' ? rows.get(target.id)?.workspace.title ?? target.label : target.label}</strong>
            {' '}{refusalText(editRefusal)}{' '}
            {snapshot.retryEdit !== undefined ? (
              <Button size="sm" variant="secondary" isDisabled={snapshot.restoreUnavailable === true} onPress={() => { void snapshot.retryEdit?.() }}>
                {snapshot.retryReady ? 'Retry edit' : 'Refresh for retry'}
              </Button>
            ) : null}
          </div>
        ))}
        <RevealSelectedAgent
          selection={selection}
          treeRef={treeRef}
          items={collectionCache.items}
          revealKey={filters.query}
          expandAncestors={(id) => {
            const ancestors = new Set<string>()
            let parent = liveAncestor(doc, doc.placements[id]?.folder ?? null)
            if (parent === null) ancestors.add(unfiled)
            while (parent !== null) {
              ancestors.add(parent)
              parent = parents.get(parent) ?? null
            }
            const next = collapsedIds.filter((key) => !ancestors.has(key))
            if (next.length !== collapsedIds.length) setCollapsedIds(next)
          }}
        />
        {visibleFolders.length === 0 && visibleUnfiled.length === 0 ? (
          <p {...stylex.props(styles.notice)}>No agents match these filters.</p>
        ) : null}
        <div role="status" {...stylex.props(styles.notice)}>
          {snapshot.phase === 'fixture' ? (
            'Fixture folders · local preview'
          ) : snapshot.phase === 'synced' ? (
            snapshot.detail
          ) : snapshot.phase === 'pending' ? (
            'Syncing folder edits…'
          ) : snapshot.phase === 'connecting' ? (
            'Connecting to Fractal folders…'
          ) : (
            <>
              {snapshot.detail}{' '}
              {snapshot.retryEdit !== undefined ? (
                <Button size="sm" variant="secondary" isDisabled={snapshot.restoreUnavailable === true} onPress={() => { void snapshot.retryEdit?.() }}>
                  {snapshot.retryReady ? 'Retry edit' : 'Refresh for retry'}
                </Button>
              ) : (
                <Button size="sm" variant="secondary" {...(snapshot.retry === undefined ? {} : { onPress: snapshot.retry })}>
                  Refresh
                </Button>
              )}
            </>
          )}
        </div>
      </section>
    )
  },
)
const descendantCount = (folder: ProjectedFolder): number =>
  folder.members.length + folder.folders.reduce((total, child) => total + descendantCount(child), 0)

/** Ignore presentation-only fields when deciding whether React Aria must rebuild its collection. */
// oxlint-disable-next-line overeng/named-args -- Fixed positional (feed, feed, filters) comparator ABI.
export const sameFolderFleet = (
  left: Feed<Fleet>,
  right: Feed<Fleet>,
  filters: SidebarFilters,
): boolean => {
  if (left._tag !== 'Observed' || right._tag !== 'Observed') return left._tag === right._tag
  const status =
    filters.needsMe ||
    filters.hideEnded ||
    filters.hideRetired ||
    filters.statuses.length > 0 ||
    filters.sort === 'status'
  // Facet predicates read raw reported facts; hideEnded also reads the raw lifecycle state.
  const facets = parseSidebarQuery(filters.query).facets
  const repo = facets.some((facet) => facet.field === 'repo')
  const tool = facets.some((facet) => facet.field === 'tool')
  const state = filters.hideEnded || facets.some((facet) => facet.field === 'state')
  return (
    left.value.agents.length === right.value.agents.length &&
    left.value.agents.every((agent, index) => {
      const next = right.value.agents[index]
      return (
        next !== undefined &&
        agent.ref === next.ref &&
        (!(status || state) ||
          agentStatus({ agent, fleet: left }) === agentStatus({ agent: next, fleet: right })) &&
        (!state || (agent.state === next.state && agent.status === next.status && agent.activity === next.activity)) &&
        (!tool || agent.harness === next.harness) &&
        (!repo ||
          (agent.checkout._tag === 'Known' ? agent.checkout.value.repository : undefined) ===
            (next.checkout._tag === 'Known' ? next.checkout.value.repository : undefined)) &&
        (filters.query.trim() === '' || agent.description === next.description) &&
        (filters.sort !== 'activity' ||
          (agent.lastActivityAt._tag === next.lastActivityAt._tag &&
            (agent.lastActivityAt._tag === 'Unknown' ||
              (next.lastActivityAt._tag === 'Known' && agent.lastActivityAt.value === next.lastActivityAt.value))))
      )
    })
  )
}

type AgentRowWorkspace = Pick<Workspace, 'id' | 'title' | 'host' | 'unread'>
interface AgentFolderTreeItem extends SidebarTreeItem<AgentFolderTreeItem> {
  readonly render: (state: Parameters<SidebarTreeProps<AgentFolderTreeItem>['renderRow']>[1]) => React.ReactNode
  readonly agentRef?: string
  readonly onAction?: () => void
  readonly onHover?: () => void
}
interface CachedAgentElement {
  readonly workspace: AgentRowWorkspace
  readonly element: AgentFolderTreeItem
}
interface CachedFolderElement {
  readonly name: string
  readonly count: number
  readonly content: AgentFolderTreeItem['render']
  readonly children: readonly AgentFolderTreeItem[]
  readonly element: AgentFolderTreeItem
}
// oxlint-disable-next-line overeng/named-args -- Retained-item array comparator; fixed positional Equivalence shape.
const sameElements = (
  left: readonly AgentFolderTreeItem[],
  right: readonly AgentFolderTreeItem[],
): boolean =>
  left.length === right.length && left.every((element, index) => element === right[index])

// Presentation changes (query highlights, spend and selection) must not replace RAC items.
const BoundFolderPresentation = React.createContext<AgentFoldersProps | null>(null)
const BoundFolderRow = ({ node }: { readonly node: SidebarNode }) => {
  const props = React.useContext(BoundFolderPresentation)
  if (props === null) throw new Error('Bound folder rows require their presentation provider')
  const status = props.rowStatus?.get(node.id)
  return <div data-sidebar-content="" {...stylex.props(styles.treeContent)}>
    <AriaButton slot="drag" aria-label={`Move ${node.label}`} {...stylex.props(styles.drag)} />
    {node._tag === 'Agent' ? props.renderAgent?.(node) : (
      <div {...stylex.props(styles.folder)}>
        <span {...stylex.props(styles.folderLabel)}>{node.label}</span>
        {props.renderFolderSignal?.(node)}
      </div>
    )}
    {status?._tag === 'Pending' ? <span role="status">Saving folder layout…</span> : null}
    {status?._tag === 'Refused' ? <span role="status">{status.reason}{status.onRetry === undefined ? null : <AriaButton onPress={status.onRetry}>Try again</AriaButton>}</span> : null}
  </div>
}

/** The live host owns roster presentation and arrangement edits; RAC owns hierarchy and focus. */
const BoundAgentFolders = (props: AgentFoldersProps) => {
  const treeRef = React.useRef<SidebarTreeHandle>(null)
  const { expanded, foldersById } = React.useMemo(() => {
    const expanded = new Set<Key>()
    const foldersById = new Map<string, SidebarNode>()
    const index = (nodes: readonly SidebarNode[]): void => {
      for (const node of nodes) {
        foldersById.set(node.id, node)
        if (node._tag !== 'Agent') {
          if (!node.collapsed) expanded.add(node.id)
          index(node.children)
        }
      }
    }
    index(props.tree)
    return { expanded, foldersById }
  }, [props.tree])
  const dropTarget = React.useCallback((target: { readonly type: string; readonly key?: Key; readonly dropPosition?: string }): SidebarMoveTarget | undefined => {
    if (target.type === 'root') return { _tag: 'Root', index: (props.orderingTree ?? props.tree).filter(node => node._tag === 'Folder').length }
    if (target.key === undefined) return undefined
    const node = foldersById.get(String(target.key))
    if (node === undefined) return undefined
    if (target.dropPosition === 'on') return node._tag === 'Group' ? { _tag: 'Unfiled' } : node._tag === 'Folder' ? { _tag: 'Into', folder: node.id } : undefined
    return { _tag: target.dropPosition === 'before' ? 'Before' : 'After', sibling: node.id }
  }, [foldersById, props.orderingTree, props.tree])
  const applyDrop = React.useCallback(async (event: { readonly target: { readonly type: string; readonly key?: Key; readonly dropPosition?: string }; readonly items: Parameters<NonNullable<DragAndDropOptions['onItemDrop']>>[0]['items'] }): Promise<void> => {
    const target = dropTarget(event.target)
    const item = event.items[0]
    if (target === undefined || item?.kind !== 'text' || event.items.length !== 1) return
    const id = await item.getText('application/x-fractal-sidebar-row')
    if ('ok' in props.canDrop([id], target)) props.onMove({ items: [id], target })
  }, [dropTarget, props.canDrop, props.onMove])
  const dragOptions = React.useMemo<DragAndDropOptions<AgentFolderTreeItem>>(() => ({
    getItems: keys => [...keys].filter(key => foldersById.get(String(key))?._tag !== 'Group').map(key => ({ 'application/x-fractal-sidebar-row': String(key), 'text/plain': foldersById.get(String(key))?.label ?? String(key) })),
    acceptedDragTypes: ['application/x-fractal-sidebar-row'],
    getAllowedDropOperations: () => ['move'],
    getDropOperation: (target, types) => {
      const intent = dropTarget(target)
      return intent !== undefined && types.has('application/x-fractal-sidebar-row') ? 'move' : 'cancel'
    },
    onItemDrop: applyDrop,
    onInsert: applyDrop,
    onRootDrop: event => applyDrop({ ...event, target: { type: 'root' } }),
  }), [foldersById, dropTarget, applyDrop])
  const { dragAndDropHooks } = useDragAndDrop<AgentFolderTreeItem>(dragOptions)
  const items = React.useMemo(() => {
    const render = (node: SidebarNode, depth: number): AgentFolderTreeItem => ({
      id: node.id, depth, textValue: node.label,
      accessibleName: props.accessibleNames?.get(node.id),
      agentRef: node._tag === 'Agent' ? node.subject : undefined,
      onAction: node._tag === 'Agent' ? () => props.onSelect?.(node.id) : undefined,
      children: node._tag === 'Agent' ? undefined : node.children.map(child => render(child, depth + 1)),
      render: () => <BoundFolderRow node={node} />,
    })
    return props.tree.map(node => render(node, 0))
  }, [props.tree, props.onSelect, props.accessibleNames])
  const expandAncestors = (selected: string) => {
    const path: Exclude<SidebarNode, { readonly _tag: 'Agent' }>[] = []
    const find = (nodes: readonly SidebarNode[]): boolean => {
      for (const node of nodes) {
        if (node._tag === 'Agent') { if (node.id === selected) return true }
        else {
          path.push(node)
          if (find(node.children)) return true
          path.pop()
        }
      }
      return false
    }
    if (find(props.tree)) for (const node of path) if (node.collapsed) props.onToggleCollapsed({ id: node.id, collapsed: false })
  }
  const onExpandedChange = React.useCallback((keys: Set<Key>) => {
    for (const node of foldersById.values()) if (node._tag !== 'Agent' && node.collapsed === keys.has(node.id)) props.onToggleCollapsed({ id: node.id, collapsed: !keys.has(node.id) })
  }, [foldersById, props.onToggleCollapsed])
  return <BoundFolderPresentation.Provider value={props}>
    <AgentFolderTree items={items} expanded={expanded} onExpandedChange={onExpandedChange} treeRef={treeRef} dragAndDropHooks={dragAndDropHooks} />
    <RevealAgentRow selected={props.selectedId ?? ''} treeRef={treeRef} items={items} revealKey={props.revealKey} expandAncestors={expandAncestors} />
    {props.treeStatus}
    {props.unavailable === undefined ? null : <div role="status">{props.unavailable}</div>}
  </BoundFolderPresentation.Provider>
}

export const AgentFolders = (props: AgentFoldersProps | React.ComponentProps<typeof LegacyAgentFolders>) =>
  'tree' in props ? <BoundAgentFolders {...props} /> : <LegacyAgentFolders {...props} />
interface FolderTreeCollectionCache {
  items: readonly AgentFolderTreeItem[]
  folderIds: readonly string[]
  onExpandedChange?: SidebarTreeProps<AgentFolderTreeItem>['onExpandedChange']
}


// The kit owns indentation, hierarchy keys, measurement, sticky ancestors and scrolling.
const renderFolderTreeRow: SidebarTreeProps<AgentFolderTreeItem>['renderRow'] = (item, state) =>
  <div data-wf-agent-ref={item.agentRef} onMouseEnter={item.onHover}>
    {item.render(state)}
  </div>

/** Keep the collection boundary stable while keyed rows subscribe to presentation separately. */
const AgentFolderTree = React.memo(({
  items, expanded, onExpandedChange, treeRef, dragAndDropHooks,
}: {
  readonly items: readonly AgentFolderTreeItem[]
  readonly expanded: ReadonlySet<Key>
  readonly onExpandedChange: SidebarTreeProps<AgentFolderTreeItem>['onExpandedChange']
  readonly treeRef: React.RefObject<SidebarTreeHandle | null>
  readonly dragAndDropHooks?: TreeProps<AgentFolderTreeItem>['dragAndDropHooks']
}) => {
  const actions = React.useMemo(() => {
    const indexed = new Map<Key, AgentFolderTreeItem>()
    const visit = (nodes: readonly AgentFolderTreeItem[]) => {
      for (const node of nodes) { indexed.set(node.id, node); if (node.children !== undefined) visit(node.children) }
    }
    visit(items)
    return indexed
  }, [items])
  return <div {...stylex.props(styles.treeViewport)}>
    <SidebarTree ref={treeRef} aria-label="Agents and folders" items={items} expandedKeys={expanded} onExpandedChange={onExpandedChange}
      renderRow={renderFolderTreeRow} onAction={key => actions.get(key)?.onAction?.()} dragAndDropHooks={dragAndDropHooks} />
  </div>
})
AgentFolderTree.displayName = 'AgentFolderTree'

type RevealAgentRowProps = {
  readonly selected: string
  readonly treeRef: React.RefObject<SidebarTreeHandle | null>
  readonly items: readonly AgentFolderTreeItem[]
  readonly revealKey?: string
  readonly expandAncestors: (id: string) => void
}
const RevealSelectedAgent = (props: Omit<RevealAgentRowProps, 'selected'> & { readonly selection: Atom.Atom<string> }) =>
  <RevealAgentRow {...props} selected={useAtomValue(props.selection)} />

const RevealAgentRow = ({ selected, treeRef, items, revealKey = '', expandAncestors }: RevealAgentRowProps) => {
  const revealed = React.useRef<string | undefined>(undefined)
  const expand = React.useEffectEvent(expandAncestors)
  // Row presentation is not a reveal request. Only selection, ancestry or search changes are.
  const ancestry: Key[] = []
  const find = (nodes: readonly AgentFolderTreeItem[]): boolean => {
    for (const node of nodes) {
      if (node.id === selected) return true
      ancestry.push(node.id)
      if (node.children !== undefined && find(node.children)) return true
      ancestry.pop()
    }
    return false
  }
  const present = find(items)
  const request = JSON.stringify([selected, ancestry, revealKey])
  React.useLayoutEffect(() => {
    if (selected === '' || !present || revealed.current === request) return
    expand(selected)
    treeRef.current?.reveal(selected)
    revealed.current = request
  }, [selected, present, request, treeRef])
  return null
}

const noAttention: readonly string[] = []
const identityFleet = (fleet: Feed<Fleet>): Feed<Fleet> => fleet
// oxlint-disable-next-line overeng/named-args -- Retained-ref array comparator; fixed positional Equivalence shape.
const sameRefs = (left: readonly string[], right: readonly string[]): boolean =>
  left.length === right.length && left.every((ref, index) => ref === right[index])

/** Presentation-only roster ticks update their keyed row; labels separately refresh collection metadata. */
const AgentFolderRow = React.memo(
  ({
    workspace,
    selection,
    dispatch,
    query,
    multipleHosts,
  }: {
    readonly workspace: AgentRowWorkspace
    readonly selection: Atom.Atom<string>
    readonly dispatch: (action: WindowAction) => void
    readonly query: string
    readonly multipleHosts: boolean
  }) => {
    const feed = useAgent(workspace.id)
    const agent = feed._tag === 'Observed' ? feed.value : undefined
    const status = agentStatus({ agent, fleet: feed })
    const now = useNow()
    const description = agent?.description?.trim() ?? ''
    const activityAt = agent?.lastActivityAt._tag === 'Known' ? agent.lastActivityAt.value : undefined
    const activate = () => {
      dispatch({ _tag: 'SelectWorkspace', id: workspace.id })
      dispatch({
        _tag: 'Layout',
        workspace: workspace.id,
        action: {
          _tag: 'Open',
          input: Schema.decodeSync(SubjectAddress)({ ref: workspace.id, presentation: 'detail' }),
        },
      })
    }
    const highlight = (text: string) => {
      if (query.trim() === '') return text
      const positions = matchPositions({ query, text })
      let offset = 0
      return [...text].map((character, index) => {
        const start = offset
        offset += character.length
        return positions.has(index) ? (
          <mark key={start} {...stylex.props(styles.highlight)}>
            {character}
          </mark>
        ) : (
          character
        )
      })
    }
    return (
      <CurrentAgent selection={selection} agentRef={workspace.id} status={status}>
        {(current) => (
          <>
            <AgentUsageRow
              agentRef={workspace.id}
              onOpen={activate}
              onDetails={() => openAgentUsage({ workspace, dispatch })}
            >
              {/* Measure presentation, not React Aria's independent focus/drag-button commits. */}
              <RenderProfiler id={`sidebar-agent/${workspace.id}`}>
                <div
                  data-observation-freshness={feed._tag === 'Observed' ? feed.freshness : undefined}
                  {...stylex.props(styles.rowContent)}
                >
                  <div {...stylex.props(styles.line)}>
                    <span
                      title={
                        agent?.statusSince === undefined
                          ? 'Status boundary unavailable; last activity is not time in status'
                          : `Observed status since ${new Date(agent.statusSince).toLocaleString()} · ${compactTime({ at: agent.statusSince, now })} in status`
                      }
                    >
                      <StatusIcon status={status} />
                    </span>
                    <span {...stylex.props(styles.nameSpace)}>
                      <span {...stylex.props(styles.nameSlot)}>
                        <span
                          title={`${workspace.title} · ${workspace.host}`}
                          aria-current={current ? 'page' : undefined}
                          {...stylex.props(styles.name)}
                        >
                          {highlight(workspace.title)}
                        </span>
                      </span>
                    </span>
                    {workspace.unread.length > 0 ? (
                      <span
                        aria-label={`${workspace.unread.length} unread`}
                        {...stylex.props(styles.count)}
                      >
                        {workspace.unread.length}
                      </span>
                    ) : null}
                    {activityAt === undefined ? null : (
                      <time
                        dateTime={new Date(activityAt).toISOString()}
                        title={`Last activity ${new Date(activityAt).toLocaleString()}`}
                        {...stylex.props(styles.meta)}
                      >
                        {compactTime({ at: activityAt, now })}
                      </time>
                    )}
                  </div>
                  <div {...stylex.props(styles.secondLine)}>
                    {description === '' ? null : (
                      <span title={description} {...stylex.props(styles.description)}>
                        {highlight(description)}
                      </span>
                    )}
                    <span {...stylex.props(styles.meta, styles.signals)}>
                      <AgentUsageSignals agentRef={workspace.id} />
                      {multipleHosts ? (
                        <span title={`Host ${workspace.host}`} {...stylex.props(styles.host)}>
                          {workspace.host}
                        </span>
                      ) : null}
                    </span>
                  </div>
                </div>
              </RenderProfiler>
            </AgentUsageRow>
            <ArrangementEditFeedback target={workspace.id} />
          </>
        )}
      </CurrentAgent>
    )
  },
)

/** Keyed feedback subscribes independently of the Tree's cached collection elements. */
export const ArrangementEditFeedback = ({ target }: { readonly target: string }) => {
  const source = useDataSource()
  const refusal = useAtomValue(source.mode === 'fixtures' ? fixtureFolders : folders,
    (snapshot) => snapshot.refusal?.targets.includes(target) ? snapshot.refusal : undefined)
  if (refusal === undefined) return null
  return (
    <span role="status" data-wf-refusal-reason={refusal.reason._tag} data-wf-refusal-code={refusal.reason.code} {...stylex.props(styles.notice)}>{refusalText(refusal)}</span>
  )
}

/** Only the previous and next selected rows subscribe to the window's current agent. */
const CurrentAgent = ({
  selection,
  agentRef,
  status,
  children,
}: {
  readonly selection: Atom.Atom<string>
  readonly agentRef: string
  readonly status: AgentStatus
  readonly children: (current: boolean) => React.ReactNode
}) => {
  const current = useAtomValue(selection, (selected) => selected === agentRef)
  return (
    <div
      {...stylex.props(
        styles.row,
        styles.rail,
        status === 'working' && styles.workingRail,
        status === 'waiting' && styles.waitingRail,
        status === 'stale' && styles.staleRail,
        current && styles.selected,
      )}
    >
      {children(current)}
    </div>
  )
}
