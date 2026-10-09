import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Tabs, TabList, Tab, TabPanel, Button as AriaButton, Link, DialogTrigger, ModalOverlay, Modal, Heading, Dialog } from 'react-aria-components'
import { useSeparator } from 'react-aria'
import { RegistryContext, RegistryProvider, useAtomSet, useAtomValue } from '@effect/atom-react'
import {
  surfaceVars as sf,
  textVars as tx,
  borderVars as bd,
  accentVars as ac,
  typeVars as t,
  radiusVars as r,
  spaceVars as s,
  geometryVars as g,
  geometryNumbers as geo,
  statusVars as st,
} from '../composition-tokens.stylex'
import { lightThemeWithoutStatus, lightStatusTheme } from '../composition-theme'
import { Icon } from '../composition/Icons'
import { Tooltip } from '../composition/Controls'
import { Transcript } from '../composition/Transcript'
import { DiffPanel, type DiffRevealRequest } from '../composition/DiffPanel'
import { EmbraceRuntimeProvider } from '../EmbraceRuntime'
import { Composer } from '../composition/Composer'
import { ViewportStore, ViewportStoreContext } from '../EmbraceScrollViewport'
import {
  addTabAtPath,
  findGroupPath,
  layoutPaneKeys,
  group,
  paneKey,
  paneKind,
  paneTitle,
  replaceAtPath,
  splitGroupAtPath,
  removeGroupAtPath,
  nodeAtPath,
  walkSplits,
  type LayoutPath,
  type SplitLayout,
  type TabGroupLayout,
  type WorkbenchLayout,
  type WorkbenchPane,
} from './workbench-model'
import { clampRatio, defaultRatio, initialRatioValues, splitRatioFamily, storeActiveTab, storeLayout } from './workbench-state'
import type { WorkbenchResources } from './workbench-model'
import { AgentDropSurface } from './AgentDropSurface'
import { agentPaneKey, openAgentAtPath, type AgentPlacement } from './agent-drag'
import { RenderProfiler } from '../perf/RenderProfiler'
import { developmentMeasurements, incrDebug } from '../perf/measurement'
import { usePaneLayer, PaneLayer, PaneSlot, PaneSlotsProvider, PaneCloseContext, usePaneSlotKey } from './PaneLayer'
import { WorkbenchPresentationContext, type WorkbenchAppearance, type WorkbenchPaneDetails } from './workbench-appearance'


/** One agent workspace: a binary split tree of tab groups. */
export interface WorkbenchProps {
  layout: WorkbenchLayout
  resources: WorkbenchResources
  /** Device-local workspace id; keys the persisted ratios and active tabs. */
  workspaceId?: string
  scheme?: 'dark' | 'light'
  style?: stylex.StyleXArray<stylex.CompiledStyles>
  statusTheme?: stylex.CompiledStyles
  appearance?: WorkbenchAppearance
  /** Contextual landmark names when several workbenches share one canvas. */
  landmarkContext?: string
  previewPlacement?: AgentPlacement
  describePane?: (pane: WorkbenchPane) => WorkbenchPaneDetails
  /** Host-owned file reveal command. Change sequence to reveal the same file again. */
  revealRequest?: DiffRevealRequest
  /**
   * App-owned pane renderer: return a node to replace the built-in view for
   * a pane (e.g. the full thread surface with composer), or undefined to use
   * the built-in kind views. Runs before the kind switch.
   */
  renderPane?: (pane: WorkbenchPane) => React.ReactNode
  /** Hide the tab bar of single-tab groups: the main app keeps its own header. */
  hideSingleTabBar?: boolean
  focusedPaneKey?: string
  onPaneSelect?: (key: string) => void
  /** Structure changed (tab added/closed, group split): the workspace-write boundary. */
  onLayoutChange?: (layout: WorkbenchLayout) => void
  /** A split ratio was committed (pointerup or keyboard): the workspace-write boundary. */
  onRatioCommit?: (path: LayoutPath, ratio: number) => void
  /** Terminal sessions are opened in the caller's bottom drawer, not a new split; a ref selects that exact session. */
  onOpenTerminal?: (ref?: string) => void
  /** Per-conversation scroll memory; defaults to one this Workbench owns. Either way it is cleared on unmount. */
  viewportStore?: ViewportStore
}

export function Workbench({ layout, resources, workspaceId = 'default', scheme = 'dark', style, statusTheme, renderPane, hideSingleTabBar = false, focusedPaneKey, onPaneSelect, onLayoutChange, onRatioCommit, appearance, landmarkContext, previewPlacement, describePane, revealRequest, onOpenTerminal, viewportStore: providedStore }: WorkbenchProps) {
  const [seed] = React.useState(() => initialRatioValues(workspaceId, layout))
  const [ownStore] = React.useState(() => new ViewportStore())
  const viewportStore = providedStore ?? ownStore
  React.useLayoutEffect(() => {
    viewportStore.open()
    return () => viewportStore.dispose()
  }, [viewportStore])
  // Parent layout effects run after every viewport's park/save in the same commit, so a
  // conversation closed in this commit is dropped rather than resurrected by its own save.
  React.useLayoutEffect(() => { viewportStore.retain(new Set(layoutPaneKeys(layout))) }, [layout, viewportStore])
  const [localFocus, setLocalFocus] = React.useState<string>()
  const [diffReveal, setDiffReveal] = React.useState<{ path: string; sequence: number }>()
  const selectPane = React.useCallback((key: string) => { setLocalFocus(key); onPaneSelect?.(key) }, [onPaneSelect])
  // Unchanged branches retain their React Aria providers; commands still read the latest root.
  const currentLayout = React.useRef(layout)
  currentLayout.current = layout
  const changeLayout = React.useCallback((next: WorkbenchLayout) => {
    storeLayout(workspaceId, next)
    currentLayout.current = next
    onLayoutChange?.(next)
  }, [workspaceId, onLayoutChange])
  const currentFocus = React.useRef(focusedPaneKey ?? localFocus)
  currentFocus.current = focusedPaneKey ?? localFocus
  const previewPath = previewPlacement === undefined ? undefined : findGroupPath(layout)
  const openDiff = React.useCallback((path?: string) => {
    const uri = path === undefined ? resources.diffs.keys().next().value : [...resources.diffs].find(([, diff]) => diff.path === path || diff.branchFiles?.some(file => file.path === path) || diff.currentTurnFiles?.some(file => file.path === path))?.[0]
    if (uri === undefined) return
    const existing = findGroupPath(currentLayout.current, uri)
    if (existing === undefined) {
      const source = findGroupPath(currentLayout.current, currentFocus.current) ?? findGroupPath(currentLayout.current)
      if (source !== undefined) changeLayout(splitGroupAtPath(currentLayout.current, source, 'right', { uri }))
    }
    setDiffReveal(previous => ({ path: path ?? resources.diffs.get(uri)!.path, sequence: (previous?.sequence ?? 0) + 1 }))
    selectPane(uri)
  }, [resources.diffs, changeLayout, selectPane])
  const revealPath = revealRequest?.path
  const revealSequence = revealRequest?.sequence
  React.useLayoutEffect(() => {
    if (revealPath !== undefined) openDiff(revealPath)
  }, [revealPath, revealSequence, openDiff])
  const panes = usePaneLayer(layout, workspaceId, focusedPaneKey ?? localFocus)
  const presentation = React.useMemo(() => ({ appearance, landmarkContext, previewPlacement, previewPath, describePane, onOpenTerminal, onOpenDiff: openDiff, diffReveal, isSplit: layout.kind === 'split' }), [appearance, landmarkContext, previewPlacement, previewPath, describePane, onOpenTerminal, openDiff, diffReveal, layout.kind])
  return (
    <RegistryProvider initialValues={seed}>
      <WorkbenchPresentationContext.Provider value={presentation}>
      <ViewportStoreContext.Provider value={viewportStore}>
      <div data-testid="workbench" data-scheme={scheme} {...stylex.props(styles.root, ...(scheme === 'light' ? lightThemeWithoutStatus : []), statusTheme ?? (scheme === 'light' ? lightStatusTheme : undefined), style)}>
        <PaneSlotsProvider value={panes.slots}>
        <LayoutNode node={layout} path="0" layout={currentLayout} workspaceId={workspaceId} resources={resources} renderPane={renderPane} hideSingleTabBar={hideSingleTabBar} focusedPaneKey={focusedPaneKey ?? localFocus} onPaneSelect={selectPane} onLayoutChange={onLayoutChange === undefined ? undefined : changeLayout} onRatioCommit={onRatioCommit} />
        </PaneSlotsProvider>
        <PaneLayer visible={panes.visible} canClose={node => onLayoutChange !== undefined && !hideSingleTabBar && node.tabs.length === 1 && appearance?.chrome !== 'P2'} render={pane => <PaneHost pane={pane} resources={resources} renderPane={renderPane} />} />
      </div>
      </ViewportStoreContext.Provider>
      </WorkbenchPresentationContext.Provider>
    </RegistryProvider>
  )
}

interface NodeProps extends Pick<WorkbenchProps, 'resources' | 'renderPane' | 'focusedPaneKey' | 'onPaneSelect' | 'onLayoutChange' | 'onRatioCommit'> {
  node: WorkbenchLayout
  path: LayoutPath
  layout: React.RefObject<WorkbenchLayout>
  workspaceId: string
  hideSingleTabBar: boolean
}

const LayoutNode = React.memo(function LayoutNode(props: NodeProps) {
  return props.node.kind === 'split'
    ? <SplitNode {...props} node={props.node} />
    : <TabGroupNode {...props} node={props.node} />
})

/**
 * Binary split with a keyboard/pointer ratio separator. A drag previews the
 * ratio as a CSS variable written at most once per animation frame; the
 * committed atom, storage and the layout-change callback each fire exactly
 * once on pointerup.
 */
function SplitNode({ node, path, layout, workspaceId, resources, renderPane, hideSingleTabBar, focusedPaneKey, onPaneSelect, onLayoutChange, onRatioCommit }: NodeProps & { node: SplitLayout }) {
  const atom = splitRatioFamily(`${workspaceId}:${path}`)
  const ratio = useAtomValue(atom)
  const setRatio = useAtomSet(atom)
  const containerRef = React.useRef<HTMLDivElement>(null)
  const separatorRef = React.useRef<HTMLDivElement>(null)
  const { separatorProps } = useSeparator({ orientation: node.split === 'right' ? 'vertical' : 'horizontal', elementType: 'div' })
  const step = (geo.splitMaxRatio - geo.splitMinRatio) / 10
  const largeStep = (geo.splitMaxRatio - geo.splitMinRatio) / 2.5

  const commit = (next: number) => {
    const value = Math.round(clampRatio(next) * 1000) / 1000
    if (value === ratio) return
    setRatio(value)
    const committed = replaceAtPath(layout.current, path, { ...node, ratio: value })
    if (onLayoutChange) onLayoutChange(committed)
    else { layout.current = committed; storeLayout(workspaceId, committed) }
    onRatioCommit?.(path, value)
  }

  const onPointerDown = (event: React.PointerEvent<HTMLDivElement>) => {
    event.preventDefault()
    const element = event.currentTarget
    element.setPointerCapture(event.pointerId)
    element.setAttribute('data-resizing', '')
    const box = containerRef.current?.getBoundingClientRect()
    if (box === undefined) return
    const total = node.split === 'right' ? box.width : box.height
    if (total <= 0) return
    let latest: number | null = null
    let frame: number | null = null
    const apply = (value: number | null) => {
      if (value === null) return
      containerRef.current?.style.setProperty('--split-ratio', String(value))
      separatorRef.current?.setAttribute('aria-valuenow', String(Math.round(value * 100)))
    }
    const move = (moveEvent: PointerEvent) => {
      const position = node.split === 'right' ? moveEvent.clientX - box.left : moveEvent.clientY - box.top
      latest = clampRatio(position / total)
      if (frame === null) frame = window.requestAnimationFrame(() => { frame = null; apply(latest) })
    }
    const finish = () => {
      element.removeEventListener('pointermove', move)
      element.removeEventListener('pointerup', finish)
      element.removeEventListener('pointercancel', finish)
      element.removeAttribute('data-resizing')
      if (frame !== null) window.cancelAnimationFrame(frame)
      apply(latest)
      if (latest !== null) commit(latest)
    }
    element.addEventListener('pointermove', move)
    element.addEventListener('pointerup', finish)
    element.addEventListener('pointercancel', finish)
  }

  const onKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    const growKey = node.split === 'right' ? 'ArrowRight' : 'ArrowDown'
    const shrinkKey = node.split === 'right' ? 'ArrowLeft' : 'ArrowUp'
    const stepSize = event.shiftKey ? largeStep : step
    if (event.key === growKey) { event.preventDefault(); commit(ratio + stepSize) }
    else if (event.key === shrinkKey) { event.preventDefault(); commit(ratio - stepSize) }
    else if (event.key === 'Home') { event.preventDefault(); commit(geo.splitMinRatio) }
    else if (event.key === 'End') { event.preventDefault(); commit(geo.splitMaxRatio) }
  }

  return (
    <div ref={containerRef} style={{ '--split-ratio': String(ratio) } as React.CSSProperties} {...stylex.props(styles.split, node.split === 'below' && styles.splitBelow)}>
      <div {...stylex.props(styles.splitPane)}>
        <LayoutNode node={node.children[0]} path={`${path}.0`} layout={layout} workspaceId={workspaceId} resources={resources} renderPane={renderPane} hideSingleTabBar={hideSingleTabBar} focusedPaneKey={focusedPaneKey !== undefined && findGroupPath(node.children[0], focusedPaneKey.split(' ')[0]) !== undefined ? focusedPaneKey : undefined} onPaneSelect={onPaneSelect} onLayoutChange={onLayoutChange} onRatioCommit={onRatioCommit} />
      </div>
      <div
        {...separatorProps}
        ref={separatorRef}
        tabIndex={0}
        aria-label="Resize split panes"
        aria-valuenow={Math.round(ratio * 100)}
        aria-valuemin={Math.round(geo.splitMinRatio * 100)}
        aria-valuemax={Math.round(geo.splitMaxRatio * 100)}
        aria-valuetext={`${Math.round(ratio * 100)}%`}
        data-resizer={`split-${path}`}
        onPointerDown={onPointerDown}
        onKeyDown={onKeyDown}
        {...stylex.props(node.split === 'right' ? styles.separatorV : styles.separatorH, styles.separatorBase)}
      />
      <div {...stylex.props(styles.splitPaneGrow)}>
        <LayoutNode node={node.children[1]} path={`${path}.1`} layout={layout} workspaceId={workspaceId} resources={resources} renderPane={renderPane} hideSingleTabBar={hideSingleTabBar} focusedPaneKey={focusedPaneKey !== undefined && findGroupPath(node.children[1], focusedPaneKey.split(' ')[0]) !== undefined ? focusedPaneKey : undefined} onPaneSelect={onPaneSelect} onLayoutChange={onLayoutChange} onRatioCommit={onRatioCommit} />
      </div>
    </div>
  )
}

/** A tab strip only becomes a scroll container while its tabs really overflow; a hugging strip stays out of the way of the pane's own scroller. */
function attachTabOverflow(list: HTMLElement | null) {
  if (list === null) return
  const fit = () => {
    const scrolls = list.scrollWidth > list.clientWidth + 1
    list.style.overflowX = scrolls ? 'auto' : 'visible'
    list.style.overflowY = scrolls ? 'hidden' : 'visible'
  }
  const schedule = () => queueMicrotask(fit)
  const resize = new ResizeObserver(schedule)
  const changes = new MutationObserver(schedule)
  resize.observe(list)
  changes.observe(list, { childList: true, characterData: true, subtree: true })
  fit()
  document.fonts?.ready.then(schedule)
  requestAnimationFrame(() => requestAnimationFrame(schedule))
  return () => { resize.disconnect(); changes.disconnect() }
}

/** Tab group: React Aria tabs, closable, with split-right / split-below and new-pane actions. */
function TabGroupNode({ node, path, layout, workspaceId, resources, hideSingleTabBar, onPaneSelect, onLayoutChange }: NodeProps & { node: TabGroupLayout }) {
  const { appearance, landmarkContext, describePane, onOpenDiff, onOpenTerminal } = React.useContext(WorkbenchPresentationContext)
  const groupElement = React.useRef<HTMLDivElement>(null)
  const registry = React.useContext(RegistryContext)
  const active = usePaneSlotKey(path)
  const select = (key: string) => {
    storeActiveTab(workspaceId, path, key)
    onPaneSelect?.(key)
  }
  const change = (next: WorkbenchLayout) => onLayoutChange?.(next)
  const setSplitRatio = useAtomSet(splitRatioFamily(`${workspaceId}:${path}`))
  const openAgent = (key: string, placement: AgentPlacement) => {
    key = agentPaneKey(key)
    // A dropped terminal session reattaches in the drawer like Open terminal; it never becomes a pane.
    if (resources.terminals.state === 'supported' && resources.terminals.sessions.some(session => session.ref === key)) {
      onOpenTerminal?.(key)
      return
    }
    const next = openAgentAtPath(layout.current, path, key, placement)
    if (next === layout.current) return
    if (placement !== 'center') {
      setSplitRatio(0.5)
    }
    change(next)
    select(key)
  }

  const close = (key: string) => {
    if (onLayoutChange === undefined) return
    const remaining = node.tabs.filter(tab => paneKey(tab) !== key)
    if (remaining.length > 0) {
      change(replaceAtPath(layout.current, path, group(remaining)))
      if (key === active) select(paneKey(remaining[0]!))
      return
    }
    const parentPath = path.slice(0, path.lastIndexOf('.'))
    const parent = parentPath === '' ? null : nodeAtPath(layout.current, parentPath)
    if (parent?.kind !== 'split') {
      change(removeGroupAtPath(layout.current, path))
      return
    }
    const siblingPath = `${parentPath}.${path.endsWith('.0') ? 1 : 0}`
    const sibling = parent.children[path.endsWith('.0') ? 1 : 0]
    // The promoted subtree moves up one level: its committed ratios move with it, keyed by their new
    // paths, so the removed split's ratio never lands on a surviving split.
    const moved = walkSplits(sibling, siblingPath).map(({ path: from }) => [`${parentPath}${from.slice(siblingPath.length)}`, registry.get(splitRatioFamily(`${workspaceId}:${from}`))] as const)
    let next = removeGroupAtPath(layout.current, path)
    for (const [to, value] of moved) {
      registry.set(splitRatioFamily(`${workspaceId}:${to}`), value)
      const promoted = nodeAtPath(next, to)
      if (promoted?.kind === 'split') next = replaceAtPath(next, to, { ...promoted, ratio: value })
    }
    // Split paths the promotion vacated keep no ratio: a later split there starts from the default.
    const surviving = new Set(walkSplits(next).map(entry => entry.path))
    for (const { path: vacated } of walkSplits(parent, parentPath)) {
      if (surviving.has(vacated)) continue
      registry.set(splitRatioFamily(`${workspaceId}:${vacated}`), defaultRatio)
    }
    // Focus follows the exact pane that takes the space (its group path and displayed key), never a URI
    // another view of the same resource may share.
    const workbench = groupElement.current?.closest('[data-testid="workbench"]')
    const neighbourPath = findGroupPath(sibling, undefined, parentPath)!
    const shownKey = workbench?.querySelector<HTMLElement>(`[data-layout-path="${findGroupPath(sibling, undefined, siblingPath)}"] [data-pane-key]`)?.dataset.paneKey
    const neighbourKey = shownKey ?? layoutPaneKeys(sibling)[0]!
    change(next)
    onPaneSelect?.(neighbourKey)
    requestAnimationFrame(() => {
      const neighbour = workbench?.querySelector<HTMLElement>(`[data-layout-path="${neighbourPath}"] [data-pane-key="${CSS.escape(neighbourKey)}"]`)
      neighbour?.querySelector<HTMLElement>('[data-testid="composer-input"], [tabindex="0"], button')?.focus({ preventScroll: true })
    })
  }

  const activePane = node.tabs.find(tab => paneKey(tab) === active) ?? node.tabs[0]!
  const tabs = (
    <div onKeyDown={event => { if (event.key === 'Delete' && (event.target as HTMLElement).closest('[role="tab"]')) { event.preventDefault(); close(active) } }} {...stylex.props(styles.groupHead, appearance?.chrome === 'P1' && styles.inlineTabs, appearance?.chrome === 'P3' && node.tabs.length === 1 && styles.tabsHidden)}>
      <TabList ref={attachTabOverflow} {...stylex.props(styles.tabList)}>
        {node.tabs.map(tab => {
          const key = paneKey(tab)
          return <Tab key={key} id={key} {...stylex.props(styles.tab, key === active && styles.tabOn)}><span {...stylex.props(styles.tabLabel)}>{describePane?.(tab).title ?? paneTitle(tab)}</span></Tab>
        })}
      </TabList>
      {!appearance && <div {...stylex.props(styles.groupActions)}>
        <AriaButton aria-label={`Close ${paneTitle(activePane)}`} onPress={() => close(paneKey(activePane))} {...stylex.props(styles.ghostMd)}><Icon name="x" /></AriaButton>
        <AriaButton aria-label="New thread pane" onPress={() => { const pane = nextThreadPane(); change(addTabAtPath(layout.current, path, pane)); select(paneKey(pane)) }} {...stylex.props(styles.ghostMd)}><Icon name="plus" /></AriaButton>
        <AriaButton aria-label="Split right" onPress={() => change(splitGroupAtPath(layout.current, path, 'right', { uri: 'about:blank' }))} {...stylex.props(styles.ghostMd)}><Icon name="panel" /></AriaButton>
        <AriaButton aria-label="Split below" onPress={() => change(splitGroupAtPath(layout.current, path, 'below', { uri: 'about:blank' }))} {...stylex.props(styles.ghostMd)}><Icon name="drawer" /></AriaButton>
      </div>}
    </div>
  )

  // Single-tab groups can drop the tab strip when the host app renders its
  // own chrome (thread header, composer) through `renderPane`.
  if (hideSingleTabBar && node.tabs.length === 1) {
    return (
      <AgentDropSurface path={path} labelPrefix={workspaceId} onOpen={openAgent} isDisabled={onLayoutChange === undefined}>
      <div data-testid="tab-group" data-layout-path={path} {...stylex.props(styles.group)}>
        <PaneSlot pane={activePane} path={path} />
      </div>
      </AgentDropSurface>
    )
  }

  // P2 keeps a real tab strip above every group, including single-tab diff panes; other chromes
  // merge a single diff's title into its own surface row, so that group is a plain labelled
  // region: a TabPanel without its TabList has no tab to label it.
  if (paneKind(activePane) === 'diff' && node.tabs.length === 1 && appearance?.chrome !== 'P2') {
    return (
      <AgentDropSurface path={path} labelPrefix={workspaceId} onOpen={openAgent} isDisabled={onLayoutChange === undefined}>
      <div ref={groupElement} data-testid="tab-group" data-layout-path={path} {...stylex.props(styles.group)}>
        <div role="region" aria-label={`${describePane?.(activePane).title ?? paneTitle(activePane)}${landmarkContext ? `, ${landmarkContext}` : ''}`} {...stylex.props(styles.tabPanel)}>
          <PaneSlot pane={activePane} path={path} onClose={onLayoutChange === undefined ? undefined : () => close(active)} />
        </div>
      </div>
      </AgentDropSurface>
    )
  }

  return (
    <AgentDropSurface path={path} labelPrefix={workspaceId} onOpen={openAgent} isDisabled={onLayoutChange === undefined}>
    <Tabs
      selectedKey={active}
      onSelectionChange={key => select(String(key))}
      aria-label="Workspace panes"
      data-testid="tab-group"
      data-layout-path={path}
      {...stylex.props(styles.group)}
    >
      {paneKind(activePane) === 'diff'
        ? tabs
        : appearance?.chrome === 'P1'
          ? <GroupHeader pane={activePane} titleInTab onOpenDiff={() => onOpenDiff?.()} resources={resources}>{tabs}</GroupHeader>
          : <>{tabs}{appearance && <GroupHeader pane={activePane} titleInTab={appearance.chrome !== 'P3' || node.tabs.length > 1} onOpenDiff={() => onOpenDiff?.()} resources={resources} />}</>}
      <TabPanel id={active} {...stylex.props(styles.tabPanel)}>
        <PaneSlot pane={activePane} path={path} />
      </TabPanel>
    </Tabs>
    </AgentDropSurface>
  )
}

function GroupHeader({ pane, children, titleInTab, onOpenDiff, resources }: { pane: WorkbenchPane; children?: React.ReactNode; titleInTab: boolean; onOpenDiff: () => void; resources: WorkbenchResources }) {
  const { appearance, describePane, onOpenTerminal } = React.useContext(WorkbenchPresentationContext)
  const details = describePane?.(pane) ?? { title: paneTitle(pane) }
  const breadcrumb = details.breadcrumb?.split(' / ').filter(segment => !titleInTab || segment !== details.title).join(' / ')
  return <header data-testid="pane-header" data-header-band={appearance?.header} {...stylex.props(styles.paneHeader)}>
    {(!children || appearance?.header !== 'H3') && <div {...stylex.props(styles.headerIdentity)}>
      {!titleInTab && <span title={details.breadcrumb} {...stylex.props(styles.headerTitle)}>{details.title}</span>}
      {appearance?.header === 'H2' && breadcrumb && <span title={breadcrumb} {...stylex.props(styles.breadcrumb)}><bdi dir="ltr">{breadcrumb}</bdi></span>}
      {appearance?.header !== 'H3' && details.status && <span {...stylex.props(styles.headerStatus, details.statusTone === 'running' && styles.statusRunning, details.statusTone === 'done' && styles.statusDone, details.statusTone === 'attention' && styles.statusAttention, details.statusTone === 'danger' && styles.statusDanger)}><span aria-hidden="true" {...stylex.props(styles.statusGlyph)} />{details.status}</span>}
    </div>}
    {children}
    {appearance?.header !== 'H3' && paneKind(pane) === 'thread' && <div {...stylex.props(styles.groupActions)}>
      <Tooltip label="Open diff"><AriaButton aria-label="Open diff" isDisabled={resources.diffs.size === 0} onPress={onOpenDiff} {...stylex.props(styles.ghostMd)}><Icon name="panel" /></AriaButton></Tooltip>
      <Tooltip label="Open terminal"><AriaButton aria-label="Open terminal" isDisabled={onOpenTerminal === undefined} onPress={() => onOpenTerminal?.()} {...stylex.props(styles.ghostMd)}><Icon name="drawer" /></AriaButton></Tooltip>
      <Tooltip label="Open pull request"><Link aria-label="Open pull request" href={details.pullRequestUrl} isDisabled={details.pullRequestUrl === undefined} target="_blank" rel="noreferrer" {...stylex.props(styles.ghostMd)}><Icon name="pull-request" /></Link></Tooltip>
      <StopAgentButton title={details.title} onStop={details.onStop} />
    </div>}
  </header>
}

/** Text actions use their own button styles; a header icon's fixed width never reaches the modal. */
function StopAgentButton({ title, onStop }: { title: string; onStop?: () => void }) {
  const titleId = React.useId()
  const descriptionId = React.useId()
  return <DialogTrigger>
    <Tooltip label="Stop agent"><AriaButton aria-label="Stop agent" isDisabled={onStop === undefined} {...stylex.props(styles.ghostMd)}><Icon name="stop-circle" /></AriaButton></Tooltip>
    <ModalOverlay isDismissable {...stylex.props(styles.modalOverlay)}><Modal {...stylex.props(styles.modal)}><Dialog role="alertdialog" aria-labelledby={titleId} aria-describedby={descriptionId} {...stylex.props(styles.dialog)}>
      {({ close }) => <><Heading id={titleId} slot="title" {...stylex.props(styles.dialogTitle)}>Stop {title}?</Heading><p id={descriptionId} {...stylex.props(styles.dialogDescription)}>The current run will be interrupted. Your draft stays available.</p><div {...stylex.props(styles.dialogActions)}><AriaButton autoFocus onPress={close} {...stylex.props(styles.dialogButton)}>Cancel</AriaButton><AriaButton onPress={() => { close(); onStop?.() }} {...stylex.props(styles.dialogButton, styles.destructive)}>Stop agent</AriaButton></div></>}
    </Dialog></Modal></ModalOverlay>
  </DialogTrigger>
}

let newPaneCounter = 0
const nextThreadPane = (): WorkbenchPane => {
  newPaneCounter += 1
  return { uri: `agent:worker-${newPaneCounter}` }
}

/** Generic pane host: maps a pane key to its resource kind; `renderPane` wins when it returns a node. */
export const PaneHost = React.memo(function PaneHost({ pane, resources, renderPane }: { pane: WorkbenchPane; resources: WorkbenchResources; renderPane?: (pane: WorkbenchPane) => React.ReactNode }) {
  React.useLayoutEffect(() => {
    if (developmentMeasurements !== undefined) incrDebug(`Mounts.Pane.${paneKey(pane)}`)
  }, [pane])
  const override = renderPane?.(pane)
  if (override !== undefined) {
    return <RenderProfiler id={`Pane.${paneKey(pane)}`}><div data-pane-uri={pane.uri} data-pane-key={paneKey(pane)} {...stylex.props(styles.pane)}>{override}</div></RenderProfiler>
  }
  const kind = paneKind(pane)
  return (
    <RenderProfiler id={`Pane.${paneKey(pane)}`}>
    <div data-pane-kind={kind} data-pane-uri={pane.uri} data-pane-key={paneKey(pane)} {...stylex.props(styles.pane)}>
      {kind === 'thread' ? <ThreadPane pane={pane} resources={resources} />
        : kind === 'diff' ? <DiffPane pane={pane} resources={resources} />
        : <PlaceholderPane pane={pane} />}
    </div>
    </RenderProfiler>
  )
})

function ThreadPane({ pane, resources }: { pane: WorkbenchPane; resources: WorkbenchResources }) {
  const { appearance, describePane } = React.useContext(WorkbenchPresentationContext)
  const thread = resources.threads.get(pane.uri)
  const controls = resources.threadControls?.get(pane.uri)
  if (thread === undefined) return <PlaceholderPane pane={pane} message="This thread is not available on this device." />
  const title = describePane?.(pane).title ?? thread.title ?? paneTitle(pane)
  return <EmbraceRuntimeProvider options={thread.runtime}>
    <div data-thread-width={appearance?.width} {...stylex.props(styles.threadFrame, appearance?.width === 'W1' && fillTheme)}>
      <Transcript {...thread.transcript} title={title} viewportKey={paneKey(pane)} />
    </div>
    <div data-testid="workbench-composer-dock" {...stylex.props(styles.composerDock, dockTheme)}>
      <div {...stylex.props(styles.composerLane)}>
        <Composer agent={title} running={thread.runtime.isRunning === true} folder={controls?.folder} branch={controls?.branch} onSend={controls?.onSend} onSteer={controls?.onSteer} onStop={controls?.onStop} draftKey={`workbench.${paneKey(pane)}`} />
      </div>
    </div>
  </EmbraceRuntimeProvider>
}

function DiffPane({ pane, resources }: { pane: WorkbenchPane; resources: WorkbenchResources }) {
  const { diffReveal, landmarkContext } = React.useContext(WorkbenchPresentationContext)
  const onClose = React.useContext(PaneCloseContext)
  const diff = resources.diffs.get(pane.uri)
  if (diff === undefined) return <PlaceholderPane pane={pane} />
  return (
    <DiffPanel
      open
      width="100%"
      onClose={onClose}
      landmarkContext={landmarkContext}
      diff={diff.lines}
      path={diff.path}
      added={diff.added}
      removed={diff.removed}
      branchFiles={diff.branchFiles}
      currentTurnFiles={diff.currentTurnFiles}
      revealRequest={diffReveal}
    />
  )
}


function PlaceholderPane({ pane, message = 'No view is registered for this resource.' }: { pane: WorkbenchPane; message?: string }) {
  return (
    <div {...stylex.props(styles.paneScroll, styles.paneCenter)}>
      <p {...stylex.props(styles.placeholderUri)}>{pane.uri}</p>
      <p {...stylex.props(styles.placeholderHint)}>{message}</p>
      {pane.form !== undefined || pane.view !== undefined ? (
        <p {...stylex.props(styles.placeholderMeta)}>
          {pane.form !== undefined ? <kbd {...stylex.props(styles.chip)}>form={pane.form}</kbd> : null}
          {pane.view !== undefined ? <kbd {...stylex.props(styles.chip)}>view={pane.view}</kbd> : null}
        </p>
      ) : null}
    </div>
  )
}

const fillTheme = stylex.createTheme(g, { lane: '100%', proseMax: '90ch' })
/** The pinned dock has an opaque canvas behind it; sampling a blurred backdrop adds no visual information. */
const dockTheme = stylex.createTheme(g, { blur: '0px' })

const styles = stylex.create({
  root: { display: 'flex', width: '100%', height: '100%', minHeight: 0, minWidth: 0, overflow: 'hidden', backgroundColor: sf.canvas, color: tx.fg, fontFamily: t.fontSans, fontSize: t.uiSize, lineHeight: t.uiLeading },
  split: { display: 'flex', flexDirection: 'row', flexGrow: 1, minWidth: 0, minHeight: 0 },
  splitBelow: { flexDirection: 'column' },
  splitPane: { display: 'flex', flexDirection: 'column', minWidth: 0, minHeight: 0, overflow: 'hidden', flexBasis: 'calc(var(--split-ratio) * 100%)', flexGrow: 0, flexShrink: 0 },
  splitPaneGrow: { display: 'flex', flexDirection: 'column', minWidth: 0, minHeight: 0, overflow: 'hidden', flexGrow: 1, flexBasis: 0 },
  separatorBase: { position: 'relative', zIndex: 1, flexShrink: 0, touchAction: 'none', backgroundColor: bd.borderStrong, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: ac.primary, outlineOffset: `calc(-1 * ${g.focusOffset})` } },
  separatorV: { width: g.hairline, alignSelf: 'stretch', cursor: 'col-resize', '::after': { content: '""', position: 'absolute', insetInline: `calc(-1 * ${g.separatorHitSlop})`, insetBlock: 0 } },
  separatorH: { height: g.hairline, width: '100%', cursor: 'row-resize', '::after': { content: '""', position: 'absolute', insetInline: 0, insetBlock: `calc(-1 * ${g.separatorHitSlop})` } },
  group: { display: 'flex', flexDirection: 'column', flexGrow: 1, minWidth: 0, minHeight: 0, overflow: 'hidden' },
  groupHead: { display: 'flex', alignItems: 'center', gap: s.xs, paddingInline: s.sm, minHeight: g.controlLg, flexShrink: 0, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: bd.border },
  inlineTabs: { minHeight: 0, borderBottomWidth: 0, paddingInline: 0, flex: '0 1 auto', minWidth: 0 },
  tabsHidden: { display: 'none' },
  paneHeader: { display: 'flex', alignItems: 'center', gap: s.sm, height: g.band, boxSizing: 'border-box', flexShrink: 0, paddingInline: s.lg, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: bd.border, minWidth: 0 },
  headerIdentity: { display: 'flex', alignItems: 'center', gap: s.sm, flex: '1 1 0', minWidth: 0 },
  breadcrumb: { fontSize: t.metaSize, color: tx.fgMuted, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', direction: 'rtl', textAlign: 'left' },
  headerTitle: { fontSize: t.uiSize, color: tx.fg, fontWeight: t.weightSemibold, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  headerStatus: { display: 'inline-flex', alignItems: 'center', gap: s.xs, color: tx.fgMuted, fontSize: t.metaSize, flexShrink: 0, whiteSpace: 'nowrap' },
  statusGlyph: { width: g.statusPip, height: g.statusPip, borderRadius: '50%', backgroundColor: 'currentColor' },
  statusRunning: { color: st.runningFg },
  statusDone: { color: tx.fgMuted },
  statusAttention: { color: st.attention },
  statusDanger: { color: st.dangerFg },
  modalOverlay: { position: 'fixed', inset: 0, zIndex: 30, display: 'flex', alignItems: 'center', justifyContent: 'center', backgroundColor: sf.scrim },
  modal: { maxWidth: g.modalMax, width: `calc(100% - ${s.panel})`, backgroundColor: sf.canvas, borderWidth: g.hairline, borderStyle: 'solid', borderColor: bd.borderStrong, borderRadius: r.md, color: tx.fg },
  dialog: { padding: s.xl, fontFamily: t.fontSans, fontSize: t.uiSize },
  dialogTitle: { margin: 0, fontSize: t.headingSize, fontWeight: t.weightSemibold },
  dialogDescription: { color: tx.fgMuted, lineHeight: t.uiLeading },
  dialogActions: { display: 'flex', justifyContent: 'flex-end', gap: s.sm },
  // Inside the modal, focus is always shown: the dialog moves focus to Cancel programmatically, which
  // a pointer-opened dialog would otherwise leave without a ring.
  dialogButton: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', height: g.controlMd, width: 'auto', paddingInline: s.md, flexShrink: 0, whiteSpace: 'nowrap', fontSize: t.metaSize, fontFamily: t.fontSans, lineHeight: t.metaLeading, borderWidth: g.hairline, borderStyle: 'solid', borderColor: bd.borderStrong, borderRadius: r.control, backgroundColor: sf.controlFill, color: tx.fg, cursor: 'pointer', ':hover': { backgroundColor: sf.rowHover }, ':focus': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: ac.primary, outlineOffset: g.hairline } },
  destructive: { color: st.dangerFg, borderColor: st.danger, ':hover': { color: st.dangerFg, backgroundColor: st.dangerMuted } },
  tabList: { display: 'flex', alignItems: 'center', gap: s.xs2, flexGrow: 0, minWidth: 0, maxWidth: '100%' },
  tab: { height: g.controlSm, maxWidth: g.tabMax, display: 'inline-flex', alignItems: 'center', gap: s.xs2, paddingInline: s.sm, borderWidth: 0, backgroundColor: sf.transparent, color: tx.fgMuted, borderRadius: r.control, fontSize: t.metaSize, lineHeight: t.metaLeading, cursor: 'pointer', ':hover': { color: tx.fg }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: ac.primary, outlineOffset: g.focusOffset } },
  tabOn: { backgroundColor: sf.washSubtle, color: tx.fg },
  tabLabel: { minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  groupActions: { marginInlineStart: 'auto', display: 'flex', alignItems: 'center', gap: s.xs2, flexShrink: 0 },
  ghostMd: { width: g.controlMd, height: g.controlMd, display: 'inline-flex', alignItems: 'center', justifyContent: 'center', borderWidth: 0, backgroundColor: sf.transparent, color: tx.fgMuted, borderRadius: r.control, cursor: 'pointer', ':hover': { backgroundColor: sf.rowHover, color: tx.fg }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: ac.primary, outlineOffset: g.focusOffset } },
  tabPanel: { display: 'flex', flexDirection: 'column', flexGrow: 1, minWidth: 0, minHeight: 0, overflow: 'hidden', outlineStyle: 'none' },
  pane: { display: 'flex', flexDirection: 'column', flexGrow: 1, minWidth: 0, minHeight: 0, overflow: 'hidden' },
  paneScroll: { flexGrow: 1, minHeight: 0, overflowY: 'auto', overflowX: 'hidden' },
  paneCenter: { display: 'flex', flexDirection: 'column', alignItems: 'center', justifyContent: 'center', gap: s.sm },
  threadFrame: { flex: '1 1 0', minHeight: 0, minWidth: 0, display: 'flex', flexDirection: 'column' },
  composerDock: { flexShrink: 0, paddingBlock: s.lg, backgroundColor: sf.canvas },
  composerLane: { width: '100%', maxWidth: g.lane, boxSizing: 'border-box', paddingInline: s.lg, marginInline: 'auto' },
  placeholderUri: { fontFamily: t.fontMono, fontSize: t.codeSize, color: tx.fgMuted },
  placeholderHint: { fontSize: t.metaSize, lineHeight: t.metaLeading, color: tx.fgFaint },
  placeholderMeta: { display: 'flex', gap: s.xs },
  chip: { fontFamily: t.fontMono, fontSize: t.denseSize, color: tx.fgFaint, borderWidth: g.hairline, borderStyle: 'solid', borderColor: bd.border, borderRadius: r.sm, paddingInline: s.xs2 },
})
