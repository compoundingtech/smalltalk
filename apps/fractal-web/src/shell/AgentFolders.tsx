import { useAtom, useAtomValue } from '@effect/atom-react'
import * as stylex from '@stylexjs/stylex'
import { Schema } from 'effect'
import type * as Atom from 'effect/reactivity/Atom'
import * as React from 'react'
import { useLocale } from 'react-aria'
import {
  Button as AriaButton,
  ListLayout,
  Tree,
  TreeItem,
  TreeItemContent,
  Virtualizer,
  type Key,
  type TreeItemRenderProps,
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
import { fixtureFolders } from '../folders/fixture.ts'
import { WfIcon } from '../icons/WfIcon.tsx'
import { SubjectAddress } from '../resources/contract.ts'
import { AgentUsageRow, AgentUsageSignals, openAgentUsage } from './AgentUsage.tsx'
import { filterFolders, matchesRow, sortMembers, type SidebarRow } from './sidebar/filter.ts'
import { fixtureSidebarState, sidebarState } from './sidebar/state.ts'
import type { SidebarFilters } from './sidebar/state.ts'
import { StatusIcon, agentStatus, compactTime, type AgentStatus } from './sidebar/StatusIcon.tsx'
import { SidebarToolbar } from './sidebar/Toolbar.tsx'
import type { Workspace, WindowAction } from './workspaces.ts'

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
  // The Tree itself is the bounded scroll viewport, not the full sidebar document.
  // Padding belongs to ListLayout so pointer drop coordinates and row offsets agree.
  tree: { outline: 'none', maxHeight: '60vh', overflowY: 'auto', minHeight: 0 },
  item: { outline: 'none', borderRadius: scale.radiusSm },
  focused: {
    outlineWidth: '2px',
    outlineStyle: 'solid',
    outlineColor: tokens['--ds-focus-color'],
    outlineOffset: '-2px',
  },
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
  affordance: {
    display: 'inline-flex',
    alignItems: 'center',
    justifyContent: 'center',
    width: '14px',
    height: '18px',
    flexShrink: 0,
    color: tokens['--ds-gray-900'],
    backgroundColor: 'transparent',
    border: 0,
    padding: 0,
    cursor: 'pointer',
    outlineColor: tokens['--ds-focus-color'],
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
  indent: (depth: number) => ({ marginInlineStart: `${depth * 16}px` }),
})

/** Fractal folders own membership; view-only filters, order and collapse use shared persistence. */
export const AgentFolders = React.memo(
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
    // This build renders folders read-only; edit notices stay empty until a later layer restores writes.
    const [notice] = React.useState('')
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
    const visibleFolders = filterFolders({
      folders: view.folders,
      rows: rows,
      filters: filters,
      sort: filters.sort,
    })
    const visibleUnfiled = sortMembers({
      members: view.unfiled.filter((id) =>
        matchesRow({ row: rows.get(id)!, filters: filters, context: 'Unfiled' }),
      ),
      rows: rows,
      sort: filters.sort,
    })
    const searching = filters.query.trim() !== ''
    const highlight = (text: string) => {
      if (!searching) return text
      const positions = matchPositions({ query: filters.query, text })
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
    const layout = React.useMemo(() => new ListLayout({ estimatedRowSize: 48, padding: 4 }), [])
    const treeRef = React.useRef<HTMLDivElement>(null)
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
        cached.depth === depth &&
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
      const element = (
        <TreeItem
          key={id}
          id={id}
          textValue={workspace.title}
          onAction={() => {
            dispatch({ _tag: 'SelectWorkspace', id })
            dispatch({
              _tag: 'Layout',
              workspace: id,
              action: {
                _tag: 'Open',
                input: Schema.decodeSync(SubjectAddress)({ ref: id, presentation: 'detail' }),
              },
            })
          }}
          onHoverStart={() => source.prefetchConversation?.(id)}
          data-wf-agent-ref={id}
          className={treeItemClassName}
        >
          <TreeItemContent>
            {() => (
              <AgentFolderRow
                workspace={rowWorkspace}
                selection={selection}
                dispatch={dispatch}
                depth={depth + 1}
                query={filters.query}
                multipleHosts={multipleHosts}
              />
            )}
          </TreeItemContent>
        </TreeItem>
      )
      agentElements.set(id, { workspace: rowWorkspace, depth, element })
      return element
    }
    const renderFolder = ({
      folder,
      depth,
    }: {
      readonly folder: ProjectedFolder
      readonly depth: number
    }): React.ReactElement => {
      const children = [
        ...folder.folders.map((child) => renderFolder({ folder: child, depth: depth + 1 })),
        ...folder.members.map((id) => renderAgent({ id, depth: depth + 1 })),
      ]
      const count = descendantCount(folder)
      const cached = folderElements.get(folder.id)
      const sameContent =
        cached !== undefined &&
        cached.name === folder.name &&
        cached.depth === depth &&
        cached.count === count
      if (sameContent && sameElements(cached.children, children)) return cached.element
      const content = sameContent ? (
        cached.content
      ) : (
        <TreeItemContent>
          {({ isExpanded }) => (
            <div {...stylex.props(styles.folder, styles.indent(depth))}>
              <AriaButton slot="chevron" {...stylex.props(styles.affordance)}>
                <WfIcon name={isExpanded ? 'chevronDown' : 'chevronRight'} />
              </AriaButton>
              <span {...stylex.props(styles.folderLabel)}>
                <WfIcon name={isExpanded ? 'folderOpen' : 'folder'} />
                <span {...stylex.props(styles.nameSlot)}>
                  <span title={folder.name} {...stylex.props(styles.name)}>
                    {highlight(folder.name)}
                  </span>
                </span>
              </span>
              <span {...stylex.props(styles.folderCount)}>{count}</span>
            </div>
          )}
        </TreeItemContent>
      )
      const element = (
        <TreeItem
          key={folder.id}
          id={folder.id}
          textValue={folder.name}
          hasChildItems
          className={treeItemClassName}
        >
          {content}
          {children}
        </TreeItem>
      )
      folderElements.set(folder.id, { name: folder.name, depth, count, content, children, element })
      return element
    }
    const unfiledChildren = visibleUnfiled.map((id) => renderAgent({ id, depth: 1 }))
    const cachedUnfiled = folderElements.get(unfiled)
    const unfiledContent =
      cachedUnfiled?.count === visibleUnfiled.length ? (
        cachedUnfiled.content
      ) : (
        <TreeItemContent>
          {({ isExpanded }) => (
            <div {...stylex.props(styles.folder)}>
              <AriaButton slot="chevron" {...stylex.props(styles.affordance)}>
                <WfIcon name={isExpanded ? 'chevronDown' : 'chevronRight'} />
              </AriaButton>
              <span {...stylex.props(styles.folderLabel)}>Unfiled</span>
              <span {...stylex.props(styles.folderCount)}>{visibleUnfiled.length}</span>
            </div>
          )}
        </TreeItemContent>
      )
    const unfiledElement =
      cachedUnfiled?.content === unfiledContent &&
      sameElements(cachedUnfiled.children, unfiledChildren) ? (
        cachedUnfiled.element
      ) : (
        <TreeItem
          key={unfiled}
          id={unfiled}
          textValue="Unfiled"
          hasChildItems
          className={treeItemClassName}
        >
          {unfiledContent}
          {unfiledChildren}
        </TreeItem>
      )
    folderElements.set(unfiled, {
      name: 'Unfiled',
      depth: 0,
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
      ...(visibleUnfiled.length === 0 && (visibleFolders.length > 0 || searching)
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
        if (!searching) setCollapsedIds(collectionCache.folderIds.filter((id) => !keys.has(id)))
      }
    }
    const expanded = React.useMemo(() => {
      const collapsed = new Set(collapsedIds)
      return new Set(collectionCache.folderIds.filter((id) => searching || !collapsed.has(id)))
    }, [collectionCache.folderIds, collapsedIds, searching])
    // Dynamic collections key their renderer cache by item identity; the renderer and its
    // dependencies must not capture live roster values. textValue still changes with the label,
    // including for an offscreen row, so React Aria's keyboard typeahead uses the current name.
    // React Aria owns row focus; capture its DOM event instead of an unsupported TreeItem prop.
    return (
      <section
        aria-label="Agents"
        onFocusCapture={(event) => {
          const row = event.target.closest<HTMLElement>('[data-wf-agent-ref]')
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
        <AgentFolderTree
          items={collectionCache.items}
          expanded={expanded}
          onExpandedChange={collectionCache.onExpandedChange}
          layout={layout}
          treeRef={treeRef}
        />
        <RevealSelectedAgent
          selection={selection}
          layout={layout}
          treeRef={treeRef}
          items={collectionCache.items}
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
            snapshot.readOnly ? snapshot.detail : notice
          ) : snapshot.phase === 'pending' ? (
            'Syncing folder edits…'
          ) : snapshot.phase === 'connecting' ? (
            'Connecting to Fractal folders…'
          ) : (
            <>
              {snapshot.detail}{' '}
              <Button
                size="sm"
                variant="secondary"
                {...(snapshot.retry === undefined ? {} : { onPress: snapshot.retry })}
              >
                Retry
              </Button>
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
  return (
    left.value.agents.length === right.value.agents.length &&
    left.value.agents.every((agent, index) => {
      const next = right.value.agents[index]
      return (
        next !== undefined &&
        agent.ref === next.ref &&
        (!status ||
          agentStatus({ agent, fleet: left }) === agentStatus({ agent: next, fleet: right })) &&
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
interface CachedAgentElement {
  readonly workspace: AgentRowWorkspace
  readonly depth: number
  readonly element: React.ReactElement
}
interface CachedFolderElement {
  readonly name: string
  readonly depth: number
  readonly count: number
  readonly content: React.ReactElement
  readonly children: readonly React.ReactElement[]
  readonly element: React.ReactElement
}
// oxlint-disable-next-line overeng/named-args -- Retained-element array comparator; fixed positional Equivalence shape.
const sameElements = (
  left: readonly React.ReactElement[],
  right: readonly React.ReactElement[],
): boolean =>
  left.length === right.length && left.every((element, index) => element === right[index])
const renderTreeElement = (element: React.ReactElement): React.ReactElement => element
const treeItemClassName = ({ isFocusVisible }: TreeItemRenderProps): string =>
  stylex.props(styles.item, isFocusVisible && styles.focused).className ?? ''

interface FolderTreeCollectionCache {
  items: readonly React.ReactElement[]
  folderIds: readonly string[]
  onExpandedChange?: TreeProps<React.ReactElement>['onExpandedChange']
}

/** Aria's Tree state/context must not rerender merely because the surrounding shell does. */
const AgentFolderTree = React.memo(
  ({
    items,
    expanded,
    onExpandedChange,
    layout,
    treeRef,
  }: {
    readonly items: readonly React.ReactElement[]
    readonly expanded: ReadonlySet<Key>
    readonly onExpandedChange: NonNullable<TreeProps<React.ReactElement>['onExpandedChange']>
    readonly layout: ListLayout<unknown>
    readonly treeRef: React.RefObject<HTMLDivElement | null>
  }) => {
    const { direction } = useLocale()
    const navigation = React.useRef<TreeItemRenderProps['state'] | undefined>(undefined)
    // RAC 1.21.1 Tree drops onKeyDown props: filterDOMProps(global) keeps only global
    // events, and ArrowRight then enters row controls even for an expanded folder. Resolve
    // hierarchy in the real render-prop collection and move its own roving focus instead;
    // Virtualizer still mounts/persists the keyed row, including offscreen children.
    const onKeyDownCapture = (event: React.KeyboardEvent<HTMLDivElement>) => {
      const state = navigation.current
      const target = event.target
      if (
        state === undefined ||
        !(target instanceof HTMLElement) ||
        target.getAttribute('role') !== 'row' ||
        event.altKey ||
        event.ctrlKey ||
        event.metaKey ||
        event.shiftKey
      )
        return
      const expandKey = direction === 'rtl' ? 'ArrowLeft' : 'ArrowRight'
      const collapseKey = direction === 'rtl' ? 'ArrowRight' : 'ArrowLeft'
      if (event.key !== expandKey && event.key !== collapseKey) return
      const focused = state.selectionManager.focusedKey
      const node = focused === null ? undefined : state.collection.getItem(focused)
      if (node === undefined || node === null) return
      let child: Key | undefined
      for (const candidate of state.collection.getChildren?.(node.key) ?? []) {
        if (candidate.type === 'item') {
          child = candidate.key
          break
        }
      }
      let destination: Key | undefined
      if (event.key === expandKey) {
        if (child === undefined) return
        if (state.expandedKeys.has(node.key)) destination = child
        else state.toggleKey(node.key)
      } else if (child !== undefined && state.expandedKeys.has(node.key)) {
        state.toggleKey(node.key)
      } else {
        const parent =
          node.parentKey === undefined || node.parentKey === null
            ? undefined
            : state.collection.getItem(node.parentKey)
        if (parent?.type !== 'item') return
        destination = parent.key
      }
      event.preventDefault()
      event.stopPropagation()
      if (destination !== undefined) {
        state.selectionManager.setFocused(true)
        state.selectionManager.setFocusedKey(destination)
      }
    }
    return (
      <div onKeyDownCapture={onKeyDownCapture}>
        <Virtualizer layout={layout} shouldObserveItemSize>
          <Tree
            aria-label="Agents and folders"
            ref={treeRef}
            items={items}
            selectionMode="none"
            expandedKeys={expanded}
            onExpandedChange={onExpandedChange}
            className={({ state }) => {
              navigation.current = state
              return stylex.props(styles.tree).className ?? ''
            }}
          >
            {renderTreeElement}
          </Tree>
        </Virtualizer>
      </div>
    )
  },
)

/** Selection from another surface reveals its row without subscribing the collection to selection. */
const RevealSelectedAgent = ({
  selection,
  layout,
  treeRef,
  items,
  expandAncestors,
}: {
  readonly selection: Atom.Atom<string>
  readonly layout: ListLayout<unknown>
  readonly treeRef: React.RefObject<HTMLDivElement | null>
  readonly items: readonly React.ReactElement[]
  readonly expandAncestors: (id: string) => void
}) => {
  const selected = useAtomValue(selection)
  const revealed = React.useRef<string | undefined>(undefined)
  const expand = React.useEffectEvent(expandAncestors)
  React.useLayoutEffect(() => {
    if (revealed.current === selected) return
    revealed.current = undefined
    expand(selected)
    // The collection portal commits before Virtualizer finishes its layout. Use its keyed
    // layout lookup on the next frame, never a DOM query/walk over every row.
    const frame = requestAnimationFrame(() => {
      if (layout.virtualizer === null) return
      const rect = layout.getLayoutInfo(selected)?.rect
      const tree = treeRef.current
      if (rect === undefined || tree === null) return
      revealed.current = selected
      if (rect.y < tree.scrollTop) tree.scrollTop = rect.y
      else if (rect.maxY > tree.scrollTop + tree.clientHeight)
        tree.scrollTop = rect.maxY - tree.clientHeight
    })
    return () => cancelAnimationFrame(frame)
  }, [selected, layout, treeRef, items])
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
    depth,
    query,
    multipleHosts,
  }: {
    readonly workspace: AgentRowWorkspace
    readonly selection: Atom.Atom<string>
    readonly dispatch: (action: WindowAction) => void
    readonly depth: number
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
      <CurrentAgent selection={selection} agentRef={workspace.id} status={status} depth={depth}>
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
          </>
        )}
      </CurrentAgent>
    )
  },
)

/** Only the previous and next selected rows subscribe to the window's current agent. */
const CurrentAgent = ({
  selection,
  agentRef,
  status,
  depth,
  children,
}: {
  readonly selection: Atom.Atom<string>
  readonly agentRef: string
  readonly status: AgentStatus
  readonly depth: number
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
        styles.indent(depth),
        current && styles.selected,
      )}
    >
      {children(current)}
    </div>
  )
}
