import { sidebarRow } from './sidebarRow.ts'
import { ConversationPaneFallback } from './ConversationPaneFallback.tsx'
import type * as ConversationPaneModule from './ConversationPane.tsx'
import { ConversationHeaderActions } from './ConversationHeaderActions.tsx'
import { retainPane, workspaceView, workspaceViewNotice, type RetainedPane, type WorkspaceView } from './workspaceView.ts'
import { ThreadHeaderSlotContext } from '../shell/threadHeaderSlot.tsx'
import { liveLegacyTheme } from '../ui-compat/live-theme.stylex.ts'
import { colorVars as c, typeVars as t, spaceVars as s, geometryVars as g } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import * as React from 'react'
import { createPortal } from 'react-dom'
import * as Aria from 'react-aria-components'
import * as stylex from '@stylexjs/stylex'
import { useAtom } from '@effect/atom-react'
import { Schema } from 'effect'
import * as Atom from 'effect/reactivity/Atom'
import { SidebarAgentRow, ThreadHeader, ResizableSplit, assistantDarkTheme, liveComposerDarkTheme, liveAccentTheme, compositionLightTheme } from '@smalltalk/fractal-ui/assistant-ui/shell'
import type { WorkLogCall } from '@smalltalk/fractal-ui/assistant-ui'
import { ViewportStore, ViewportStoreContext } from '../../../../packages/fractal-ui/src/assistant-ui/EmbraceScrollViewport.tsx'
import { terminalSubjectForAgent } from '../data/projections.ts'
import { useFleet, useSubjectList, useConnection, useNow, useFeedInterest, useDataSource } from '../data/react.tsx'
import { persistedAtom } from '../state/persistence.ts'
import { WorkbenchContextProvider, ResourcePanelProvider, MonitorDetailProvider, type OpenRequest } from '../shell/context.tsx'
import type { ResourcePanelState } from '../shell/state.ts'
import { afterNextPaint, type UxTelemetry } from '../telemetry/ux.ts'

// A static import would evaluate the kit Transcript/Markdown/refractor before the shell paints.
// Demand and speculative prefetch share one in-flight request; rejection releases it for retry.
let conversationPaneModule: Promise<typeof ConversationPaneModule> | undefined
const loadConversationPane = () => {
  if (conversationPaneModule === undefined) {
    conversationPaneModule = import('./ConversationPane.tsx').catch(error => {
      conversationPaneModule = undefined
      throw error
    })
  }
  return conversationPaneModule
}
const ConversationPane = React.lazy(() => loadConversationPane().then(module => ({ default: module.ConversationPane })))
let prefetchedConversationPane: typeof ConversationPane | undefined
const prefetchConversationPane = () => {
  const request = loadConversationPane()
  // Preserve this attempt's failure for the requested pane's honest, local error surface.
  // A retry creates a fresh lazy wrapper, not React's permanently rejected cached wrapper.
  prefetchedConversationPane = React.lazy(() => request.then(module => ({ default: module.ConversationPane })))
  void request.catch(() => {})
}

/** Acquire visible route demand outside Suspense so the socket read overlaps pane evaluation. */
const ConversationPaneLoadBoundary = (props: React.ComponentProps<typeof ConversationPaneModule.ConversationPane>) => {
  const source = useDataSource()
  const interest = React.useMemo(() => source.conversationInterest?.(props.agentRef), [source, props.agentRef])
  useFeedInterest({ interest, visible: props.visible ?? true })
  return <ConversationPaneCodeBoundary {...props} interest={interest} />
}

class ConversationPaneCodeBoundary extends React.Component<React.ComponentProps<typeof ConversationPaneModule.ConversationPane>> {
  state: { readonly failed: boolean; readonly Pane: typeof ConversationPane } = {
    failed: false,
    Pane: prefetchedConversationPane ?? ConversationPane,
  }

  static getDerivedStateFromError() {
    return { failed: true }
  }

  render() {
    if (this.state.failed) return <div role="alert" aria-hidden={!this.props.visible} {...stylex.props(styles.empty)}>
      <p>This conversation view could not load.</p>
      <button type="button" onClick={() => {
        const Pane = React.lazy(() => loadConversationPane().then(module => ({ default: module.ConversationPane })))
        prefetchedConversationPane = Pane
        this.setState({ failed: false, Pane })
      }}>Try again</button>
    </div>
    const Pane = this.state.Pane
    return <React.Suspense fallback={<ConversationPaneFallback agentName={this.props.agentName} visible={this.props.visible} />}>
      <Pane {...this.props} />
    </React.Suspense>
  }
}

// The kit diff viewer imports Markdown; keep it outside the shell's eager graph too.
const ChangesInspector = React.lazy(() => import('./ChangesInspector.tsx').then(module => ({ default: module.ChangesInspector })))

const sidebarRatio = persistedAtom({ key: 'round2.sidebarRatio', schema: Schema.Number, defaultValue: 256 / 1440 })
const panelRatio = persistedAtom({ key: 'round2.panelRatio', schema: Schema.Number, defaultValue: 380 / 1440 })
const sidebarClosed = persistedAtom({ key: 'round2.sidebarClosed', schema: Schema.Boolean, defaultValue: false })
const palette = persistedAtom({ key: 'round2.scheme', schema: Schema.Literals(['dark', 'light']), defaultValue: 'dark' })
const workspacePanes = Atom.family((ref: string) => persistedAtom({ key: `round2.panes.${ref}`, schema: Schema.Array(Schema.Struct({ ref: Schema.String, presentation: Schema.optional(Schema.Literals(['detail', 'overview', 'resources'])) })), defaultValue: [] }))
const selectedAgent = persistedAtom({ key: 'round2.agent', schema: Schema.String, defaultValue: '' })
const viewportWidth = () => window.innerWidth
const subscribeViewport = (callback: () => void) => {
  window.addEventListener('resize', callback)
  return () => window.removeEventListener('resize', callback)
}
const locationState = () => `${window.location.pathname}${window.location.search}`
const subscribeLocation = (callback: () => void) => {
  window.addEventListener('popstate', callback)
  return () => window.removeEventListener('popstate', callback)
}
const initialAgentFromUrl = () => {
  const encoded = window.location.pathname.match(/^\/w\/(.+)$/)?.[1]
  try {
    return encoded === undefined ? undefined : decodeURIComponent(encoded)
  } catch {
    return undefined
  }
}

/** One authoritative agent thread per workspace; local geometry never grants backend authority. */
export function LiveAgentWorkspace({ ux, onSelectConversation }: { readonly ux?: UxTelemetry; readonly onSelectConversation?: (ref: string) => void }) {
  const fleet = useFleet(), subjects = useSubjectList(), connection = useConnection(), now = useNow()
  const [storedAgent, setStoredAgent] = useAtom(selectedAgent)
  const [ratio, setRatio] = useAtom(sidebarRatio), [collapsed, setCollapsed] = useAtom(sidebarClosed), [scheme, setScheme] = useAtom(palette)
  const viewport = React.useSyncExternalStore(subscribeViewport, viewportWidth, viewportWidth)
  const location = React.useSyncExternalStore(subscribeLocation, locationState, locationState)
  const hoverOwner = React.useRef<HTMLElement | null>(null)
  const rememberHoverOwner = (event: React.SyntheticEvent<HTMLElement>) => {
    hoverOwner.current = event.target instanceof Element ? event.target.closest<HTMLElement>('[data-wf-agent-ref]') : null
  }
  React.useLayoutEffect(() => {
    // Use the row's existing Escape contract: it cancels pending hover intent as well
    // as closing the card, without replacing any roster nodes during a switch.
    hoverOwner.current?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
    hoverOwner.current = null
  }, [location])
  const agents = fleet._tag === 'Observed' ? fleet.value.agents : []
  const rosterObserved = fleet._tag === 'Observed'
  const shellCommit = React.useCallback((node: HTMLDivElement | null) => {
    if (node === null) return
    const cancelCapture = ux?.shellCommitted()
    // The shared paint scheduler posts a task after the first shell frame, not a second rAF.
    // A selected pane requests code immediately through lazy; the empty shell warms it here.
    const cancelPrefetch = afterNextPaint(prefetchConversationPane)
    return () => { cancelCapture?.(); cancelPrefetch() }
  }, [ux])
  const rosterCommit = React.useCallback((node: HTMLElement | null) => node === null || !rosterObserved ? undefined : ux?.rosterCommitted(), [ux, rosterObserved])
  const current = initialAgentFromUrl() ?? (storedAgent || agents[0]?.ref || '')
  const agent = agents.find((row) => row.ref === current)
  // Fleet projection assigns every agent this canonical subject address; it is not a
  // claim that the runtime has a terminal or that terminal input is granted.
  const terminalRef = agent?.terminal ?? terminalSubjectForAgent(current)
  const [headerSlot, setHeaderSlot] = React.useState<HTMLDivElement | null>(null)
  const [paneHost, setPaneHost] = React.useState<HTMLDivElement | null>(null)
  const [search, setSearch] = React.useState('')
  const [panes, setPanes] = useAtom(workspacePanes(current))
  const query = new URLSearchParams(location.split('?')[1])
  const selectedPane = query.get('open') ?? 'thread'
  const [resourcePanel, setResourcePanel] = React.useState<ResourcePanelState>({ expanded: false, size: 288 })
  const [diffOpen, setDiffOpen] = React.useState(false)
  const [openedTool, setOpenedTool] = React.useState<WorkLogCall | undefined>(undefined)
  const [panelFraction, setPanelFraction] = useAtom(panelRatio)
  const [dragWidth, setDragWidth] = React.useState<number | undefined>()
  const [dragPanel, setDragPanel] = React.useState<number | undefined>()
  const navigate = (ref: string, pane = 'thread') => {
    setStoredAgent(ref)
    window.history.pushState(null, '', `/w/${ref.split('/').map(encodeURIComponent).join('/')}?open=${encodeURIComponent(pane)}`)
    window.dispatchEvent(new PopStateEvent('popstate'))
  }
  const select = (ref: string) => {
    if (ref !== current) onSelectConversation?.(ref)
    setDiffOpen(false)
    setOpenedTool(undefined)
    navigate(ref)
  }
  const open = (request: OpenRequest) => {
    if (request.ref.startsWith('agent/') && request.presentation === undefined) {
      select(request.ref)
      return
    }
    const id = `${request.ref}:${request.presentation ?? 'detail'}`
    setPanes((rows) => (rows.some((row) => `${row.ref}:${row.presentation ?? 'detail'}` === id) ? rows : [...rows, { ref: request.ref, ...(request.presentation === undefined ? {} : { presentation: request.presentation }) }]))
    navigate(current, id)
  }
  const stale = fleet._tag === 'Observed' && fleet.freshness === 'stale'
  const headerRow = agent === undefined ? undefined : sidebarRow({ agent, stale, now })
  const filtered = agents.filter((row) => `${row.name} ${row.ref} ${row.host}`.toLowerCase().includes(search.toLowerCase()))
  const byRef = React.useMemo(() => new Map(subjects.map((subject) => [subject.ref, subject])), [subjects])
  const separator = selectedPane.lastIndexOf(':')
  const urlPresentation = selectedPane.slice(separator + 1)
  const urlPane: OpenRequest | undefined =
    separator > 0 && (urlPresentation === 'detail' || urlPresentation === 'overview' || urlPresentation === 'resources')
      ? { ref: selectedPane.slice(0, separator), presentation: urlPresentation }
      : undefined
  const chosen = panes.find((row) => `${row.ref}:${row.presentation ?? 'detail'}` === selectedPane) ?? urlPane
  const visiblePanes = chosen && !panes.some((row) => `${row.ref}:${row.presentation ?? 'detail'}` === selectedPane) ? [...panes, chosen] : panes
  const address = { ref: chosen?.ref ?? current, presentation: chosen?.presentation ?? 'detail' }
  const width = dragWidth ?? Math.round(ratio * viewport)
  const maxPanel = Math.max(240, viewport - (collapsed ? 40 : Math.min(width, Math.max(208, Math.min(440, viewport - 640)))) - 640)
  const panelSize = dragPanel ?? Math.min(maxPanel, Math.max(240, Math.round(panelFraction * viewport)))
  const maxSidebar = Math.max(208, Math.min(440, viewport - 640))
  const agentName = agent?.name ?? current.split('/').at(-1) ?? 'Conversation'
  // The panes are portalled from outside the shell providers: those values change on every
  // switch, and React then scans every fiber below a memoized pane for consumers, which costs
  // O(retained turns) per click. No pane reads these shell contexts.
  return (<>
    <WorkbenchContextProvider value={{ open, focusedRef: address.ref || null, subjects: byRef, platform: navigator.platform.includes('Mac') ? 'mac' : 'other' }}>
      <ResourcePanelProvider value={{ state: resourcePanel, onChange: (change) => setResourcePanel((value) => ({ ...value, ...change })) }}>
        <MonitorDetailProvider value={{ size: 380, onSizeChange: (value) => setPanelFraction(value / viewport) }}>
          <ThreadHeaderSlotContext.Provider value={headerSlot}>
            <div ref={shellCommit} data-testid="live-agent-workspace" data-scheme={scheme} {...stylex.props(styles.app, styles.legacyBridge, liveLegacyTheme, scheme === 'dark' && assistantDarkTheme, scheme === 'dark' && liveComposerDarkTheme, liveAccentTheme, scheme === 'light' && compositionLightTheme)}>
              <aside aria-label="Agents" style={{ width: collapsed ? 40 : Math.min(width, maxSidebar) }} {...stylex.props(styles.sidebar)}>
                <header {...stylex.props(styles.brand)}>
                  <Aria.Button aria-label={collapsed ? 'Expand agents' : 'Collapse agents'} onPress={() => setCollapsed(!collapsed)} {...stylex.props(styles.iconButton)}>
                    {collapsed ? '»' : '«'}
                  </Aria.Button>
                  {collapsed ? null : <strong>Fractal</strong>}
                </header>
                {collapsed ? null : (
                  <>
                    <Aria.SearchField aria-label="Search agents" value={search} onChange={setSearch} {...stylex.props(styles.search)}>
                      <Aria.Input placeholder="Search agents" {...stylex.props(styles.searchInput)} />
                    </Aria.SearchField>
                    <nav ref={rosterCommit} aria-label="Agent roster" onMouseOverCapture={rememberHoverOwner} onFocusCapture={rememberHoverOwner} {...stylex.props(styles.roster)}>
                      {filtered.map((row) => (
                        <SidebarAgentRow key={row.ref} item={sidebarRow({ agent: row, stale, now })} now={now} variant="SR-2" layout="SR2-A" glyph="SG-1" extraSignals={[]} query={search} active={current === row.ref} onOpen={() => select(row.ref)} />
                      ))}
                    </nav>
                    {fleet._tag !== 'Observed' ? (
                      <p role="status" data-wf-roster-reason={fleet._tag === 'Unavailable' ? fleet.reason : undefined} {...stylex.props(styles.notice)}>
                        {fleet._tag === 'Waiting' ? 'Waiting for the agent roster.' : fleet.reason === 'ungranted' ? 'Read access to the agent roster has not been granted.' : fleet.reason === 'unsupported' ? 'This connection does not support the agent roster.' : 'The agent roster could not be loaded; reconnect and try again.'}
                      </p>
                    ) : stale ? (
                      <p role="status" {...stylex.props(styles.notice)}>
                        Last verified roster · reconnecting
                      </p>
                    ) : null}
                    <footer {...stylex.props(styles.footer)}>
                      <Aria.Button onPress={() => setScheme(scheme === 'dark' ? 'light' : 'dark')} aria-label="Toggle color scheme" {...stylex.props(styles.iconButton)}>
                        {scheme === 'dark' ? '☀' : '☾'}
                      </Aria.Button>
                      <Aria.Button onPress={() => open({ ref: 'monitor/quota', presentation: 'detail' })} {...stylex.props(styles.textButton)}>
                        Usage
                      </Aria.Button>
                      <span {...stylex.props(styles.connection)}>{connection._tag}</span>
                    </footer>
                  </>
                )}
              </aside>
              <ResizableSplit id="live-agent-sidebar" value={width} min={208} max={maxSidebar} collapsed={collapsed} label="Agent sidebar width" onChange={(value) => { setCollapsed(false); setDragWidth(value) }} onCommit={(value) => { setRatio(value / viewport); setDragWidth(undefined) }} onToggle={() => setCollapsed(!collapsed)} onReset={() => { setCollapsed(false); setRatio(256 / viewport) }} />
              <section aria-label="Agent workspace" {...stylex.props(styles.workspace)}>
                <ThreadHeader terminalAvailable={terminalRef !== undefined} nativeActions={current === '' || selectedPane !== 'thread' ? undefined : <ConversationHeaderActions key={current} agentRef={current} />} actionPortalRef={setHeaderSlot} folder={agent?.host} title={agent?.name ?? (current || 'Select an agent')} status={headerRow?.status} statusLabel={headerRow?.statusLabel} statusSince={headerRow?.statusSince} freshness={stale ? 'stale' : agent === undefined ? 'unobserved' : 'live'} now={now} panelOpen={diffOpen} drawerOpen={selectedPane.includes('terminal/')} onTogglePanel={() => setDiffOpen((value) => !value)} onToggleDrawer={() => { if (selectedPane.startsWith('terminal/')) navigate(current); else if (terminalRef !== undefined) open({ ref: terminalRef }) }} />
                {visiblePanes.length > 0 ? (
                  <div role="toolbar" aria-label="Workspace views" {...stylex.props(styles.tabs)}>
                    <Aria.Button aria-pressed={selectedPane === 'thread'} onPress={() => navigate(current)} {...stylex.props(styles.textButton)}>
                      Thread
                    </Aria.Button>
                    {visiblePanes.map((pane) => {
                      const id = `${pane.ref}:${pane.presentation ?? 'detail'}`
                      return (
                        <Aria.Button key={id} aria-pressed={id === selectedPane} onPress={() => navigate(current, id)} {...stylex.props(styles.textButton)}>
                          {byRef.get(pane.ref)?.title ?? pane.ref.split('/').at(-1)}
                        </Aria.Button>
                      )
                    })}
                  </div>
                ) : null}
                <div ref={setPaneHost} {...stylex.props(styles.body)} />
              </section>
              {diffOpen && openedTool === undefined && current !== '' ? (
                <>
                  <ResizableSplit id="live-change-panel" reverse value={panelSize} min={240} max={maxPanel} collapsed={false} label="Change panel width" onChange={setDragPanel} onCommit={(value) => { setPanelFraction(value / viewport); setDragPanel(undefined) }} onToggle={() => setDiffOpen(false)} onReset={() => setPanelFraction(380 / viewport)} />
                  <React.Suspense fallback={<aside aria-label="Changes" style={{ width: panelSize }} {...stylex.props(styles.changesPanel)}><p role="status" {...stylex.props(styles.notice)}>Loading captured changes…</p></aside>}>
                    <ChangesInspector key={current} agentRef={current} width={panelSize} />
                  </React.Suspense>
                </>
              ) : null}
              {openedTool !== undefined && current !== '' ? (
                <>
                  <ResizableSplit id="live-tool-panel" reverse value={panelSize} min={240} max={maxPanel} collapsed={false} label="Tool detail panel width" onChange={setDragPanel} onCommit={(value) => { setPanelFraction(value / viewport); setDragPanel(undefined) }} onToggle={() => setOpenedTool(undefined)} onReset={() => setPanelFraction(380 / viewport)} />
                  <aside aria-label="Tool detail" style={{ width: panelSize }} {...stylex.props(styles.changesPanel)}>
                    <header {...stylex.props(styles.toolDetailHeader)}>
                      <strong {...stylex.props(styles.toolDetailTitle)}>{openedTool.title}{openedTool.argsSummary === undefined ? '' : ` ${openedTool.argsSummary}`}</strong>
                      <Aria.Button aria-label="Close tool detail" onPress={() => setOpenedTool(undefined)} {...stylex.props(styles.iconButton)}>×</Aria.Button>
                    </header>
                    <pre {...stylex.props(styles.toolDetailOutput)}>{openedTool.detail ?? (openedTool.status === 'running' ? 'No output received yet.' : 'No output recorded.')}</pre>
                  </aside>
                </>
              ) : null}
            </div>
          </ThreadHeaderSlotContext.Provider>
        </MonitorDetailProvider>
      </ResourcePanelProvider>
    </WorkbenchContextProvider>
    {paneHost === null ? null : createPortal(<WorkspaceBody current={current} rosterRefs={agents.map(agent => agent.ref)} view={workspaceView(chosen)} agentName={agentName} onOpenTool={setOpenedTool} ux={ux} />, paneHost)}
  </>)
}

/** The three most recently opened panes stay mounted in a stable DOM order; hidden ones skip
 * rendering through `content-visibility: hidden`, which keeps their style/layout state so a
 * switch back only reveals them. Hidden panes release visible follow demand; the SDK may
 * evict their follows. Render-time history updates avoid a second committed selection.
 * Locking a pane costs O(its rows) in the browser, so the outgoing pane is only covered by the
 * opaque shown pane during the switch frame and locked after that frame paints. */
export function WorkspaceBody({ current, rosterRefs, view, agentName, onOpenTool, ux }: { readonly current: string; readonly rosterRefs: readonly string[]; readonly view: WorkspaceView; readonly agentName: string; readonly onOpenTool: (call: WorkLogCall) => void; readonly ux?: UxTelemetry }) {
  // Reading positions belong to this surface, not its three-pane DOM cache. A new
  // surface (including reload) starts following live; evicted panes keep memory here.
  const [viewportStore] = React.useState(() => new ViewportStore())
  React.useEffect(() => {
    viewportStore.open()
    return () => viewportStore.dispose()
  }, [viewportStore])
  const [retained, setRetained] = React.useState<readonly RetainedPane[]>([])
  const [switched, setSwitched] = React.useState<{ readonly shown: string; readonly covered: string | undefined }>({ shown: current, covered: undefined })
  const visible = current !== '' && view._tag === 'Thread'
  const [visited, setVisited] = React.useState<readonly string[]>([])
  const rosterKeys = new Set(rosterRefs)
  const recent = visited.filter(ref => rosterKeys.has(ref))
  const readingKeys = visible && rosterKeys.has(current)
    ? [current, ...recent.filter(ref => ref !== current)].slice(0, 32)
    : recent
  if (visited.length !== readingKeys.length || visited.some((ref, index) => ref !== readingKeys[index])) setVisited(readingKeys)
  // Viewports save in mutation cleanup. Prune after those saves, including late
  // eviction saves, rather than allowing departed keys to re-enter the memory.
  React.useLayoutEffect(() => { viewportStore.retain(new Set(readingKeys)) })
  const latest = retained.reduce<RetainedPane | undefined>((best, pane) => best === undefined || pane.used > best.used ? pane : best, undefined)
  if (visible && (latest?.ref !== current || latest.name !== agentName)) {
    setRetained(retainPane({ panes: retained, ref: current, name: agentName, limit: 3 }))
  }
  if (visible && switched.shown !== current) setSwitched({ shown: current, covered: switched.shown })
  React.useEffect(() => {
    if (switched.covered === undefined) return
    let timer: number | undefined
    // rAF runs before the switch frame paints; the timeout runs after it.
    const frame = window.requestAnimationFrame(() => { timer = window.setTimeout(() => setSwitched(value => ({ shown: value.shown, covered: undefined }))) })
    return () => { window.cancelAnimationFrame(frame); window.clearTimeout(timer) }
  }, [switched])
  return <ViewportStoreContext.Provider value={viewportStore}>
    {retained.map(pane => {
      const shown = visible && pane.ref === current
      return <div key={pane.ref} {...stylex.props(styles.retainedPane, shown ? styles.shownPane : visible && pane.ref === switched.covered ? null : styles.hiddenPane)}>
        <ConversationPaneLoadBoundary agentRef={pane.ref} agentName={pane.name} onOpenTool={onOpenTool} ux={ux} visible={shown} />
      </div>
    })}
    {current === '' ? <div {...stylex.props(styles.empty)}>Choose an agent to open its live thread.</div>
      : view._tag === 'Thread' ? null : <p role="status" {...stylex.props(styles.empty)}>{workspaceViewNotice(view)}</p>}
  </ViewportStoreContext.Provider>
}

const styles = stylex.create({
  legacyBridge: { '--canvas': c.canvas, '--panel': c.raised, '--recess': c.message, '--ink': c.fg, '--muted': c.fgMuted, '--line': c.borderStrong, '--accent': c.primary, '--on-accent': c.onPrimary, '--selection': c.rowActive, '--good': c.done, '--warning': c.attention, '--danger': c.dangerFg, '--sans': t.fontSans, '--mono': t.fontMono },
  app: { display: 'flex', height: 'calc(100dvh - var(--wf-devbar-space, 0px))', width: '100%', overflow: 'hidden', backgroundColor: c.canvas, color: c.fg, fontFamily: t.fontSans, fontSize: t.uiSize, lineHeight: t.uiLeading },
  sidebar: { display: 'flex', flexDirection: 'column', flexShrink: 0, minHeight: 0, overflow: 'hidden', backgroundColor: c.sidebar },
  brand: { height: g.band, paddingInline: s.lg, display: 'flex', alignItems: 'center', gap: s.md, flexShrink: 0 },
  iconButton: { width: 28, height: 28, display: 'inline-flex', justifyContent: 'center', alignItems: 'center', backgroundColor: 'transparent', color: c.fgMuted, borderWidth: 0, borderRadius: 6, cursor: 'pointer' },
  textButton: { padding: '4px 8px', backgroundColor: 'transparent', color: c.fgMuted, borderWidth: 0, borderRadius: 6, cursor: 'pointer', fontSize: t.metaSize },
  search: { marginInline: s.md, padding: s.md, borderRadius: 8, backgroundColor: c.raised, flexShrink: 0 },
  searchInput: { width: '100%', minWidth: 0, borderWidth: 0, outline: 'none', backgroundColor: 'transparent', color: c.fg, fontSize: t.metaSize },
  roster: { display: 'flex', flexDirection: 'column', overflowY: 'auto', padding: s.md, minHeight: 0, flexGrow: 1, gap: 2 },
  footer: { height: 40, display: 'flex', alignItems: 'center', gap: s.md, paddingInline: s.lg, flexShrink: 0 },
  connection: { marginLeft: 'auto', fontSize: t.denseSize, color: c.fgMuted },
  workspace: { display: 'flex', flexDirection: 'column', flexGrow: 1, minWidth: 0, minHeight: 0 },
  body: { position: 'relative', display: 'flex', flexDirection: 'column', flexGrow: 1, minWidth: 0, minHeight: 0, overflow: 'hidden' },
  // Identical geometry in both states: revealing a pane must not invalidate its layout. Only
  // non-inherited properties toggle (no inert/pointer-events/visibility), so a reveal does not
  // restyle the retained subtree; content-visibility already removes it from focus and a11y.
  retainedPane: { position: 'absolute', inset: 0, display: 'flex', flexDirection: 'column', minWidth: 0, minHeight: 0, backgroundColor: c.canvas },
  shownPane: { zIndex: 1 },
  hiddenPane: { contentVisibility: 'hidden' },
  tabs: { height: 32, display: 'flex', alignItems: 'center', gap: s.xs, paddingInline: s.lg, borderBottomWidth: 1, borderBottomStyle: 'solid', borderBottomColor: c.border, flexShrink: 0 },
  notice: { padding: s.lg, fontSize: t.metaSize, color: c.fgMuted },
  empty: { position: 'relative', zIndex: 2, margin: 'auto', padding: s.section, color: c.fgMuted },
  changesPanel: { display: 'flex', flexDirection: 'column', flexShrink: 0, minWidth: 0, minHeight: 0, overflowY: 'auto', borderLeftWidth: 1, borderLeftStyle: 'solid', borderLeftColor: c.border },
  toolDetailHeader: { height: 40, display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: s.md, paddingInline: s.lg, flexShrink: 0, borderBottomWidth: 1, borderBottomStyle: 'solid', borderBottomColor: c.border },
  toolDetailTitle: { minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', fontSize: t.metaSize },
  toolDetailOutput: { margin: 0, padding: s.lg, minHeight: 0, overflowY: 'auto', fontFamily: t.fontMono, fontSize: t.denseSize, lineHeight: 1.5, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere', color: c.fgMuted },
})
