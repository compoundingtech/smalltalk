// Workshop-only composition of shipped kit parts (Transcript, WF-1, RC-3 chips, DiffPanel, Composer, ResizableSplit) with the
// milestone's own shell: a React Aria roster tree, one 32px pane header, an RC-1 subagent card and the shell states.
import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Dialog, Modal, ModalOverlay } from 'react-aria-components'
import { UNSAFE_PortalProvider } from 'react-aria'
import { ResizableSplit, usePersistedSize } from '../../assistant-ui/composition/Shell'
import { ConnectionStatus } from '../../assistant-ui/composition/SystemState'
import { DiffPanel, type DiffRevealRequest } from '../../assistant-ui/composition/DiffPanel'
import { ResourceChipV1 } from '../../assistant-ui/resources/ResourceCardV1'
import type { TranscriptTurn } from '../../assistant-ui/composition/Transcript'
import type { WorkLogCall } from '../../assistant-ui/taste/work-log'
import type { SyncStatus } from '../../assistant-ui/st3-views/sync-status'
import type { SidebarAgentRow as Row } from '../../assistant-ui/sidebar/model'
import type { DiscRefinement } from '../../assistant-ui/sidebar/SidebarStatus'
import { baselineTheme } from '../../assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from '../../assistant-ui/composition-theme'
import { borderVars as border, elevationVars as elevation, geometryNumbers, geometryVars as g, radiusVars as r, spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from '../../assistant-ui/composition-tokens.stylex'
import { MilestoneSidebar, type RosterState } from './MilestoneSidebar'
import { MilestonePalette, type PaletteCommand } from './MilestonePalette'
import { AgentResultCard, ChangesToggle, HeaderToggle, MilestonePane, MilestoneThread, type PaneTab, type ThreadView } from './MilestonePane'
import { branchFiles, changedFiles, idleAgentRef, inboxCount, lastSeen, mainAgentRef, mainTurns, milestoneNow, proseResources, reviewerCard, reviewerRef, reviewerTurns, rosterFolders, staleMainTurns } from './milestone-fixtures'

export type MilestoneState = 'slice' | 'loading' | 'empty-roster' | 'empty-thread' | 'no-match' | 'roster-error' | 'thread-unavailable' | 'stale' | 'navigation'
/** Recovery phases a Retry walks through; never a story's starting state. */
type Phase = MilestoneState | 'roster-retry' | 'thread-retry'

export interface RedesignMilestoneProps {
  readonly scheme: Scheme
  readonly state: MilestoneState
  readonly paletteScope: 'mixed' | 'agents'
  readonly glyphRefinement: DiscRefinement
  readonly sectionNav: 'footer' | 'header'
  /** Device-local key for persisted panel sizes; stories pass a per-story key so screenshots stay deterministic. */
  readonly persistKey: string
}

const rosterState: Record<Phase, RosterState> = {
  slice: 'ready', loading: 'loading', 'empty-roster': 'empty', 'empty-thread': 'ready', 'no-match': 'ready', 'roster-error': 'error',
  'roster-retry': 'loading', 'thread-unavailable': 'ready', 'thread-retry': 'ready', stale: 'stale', navigation: 'ready',
}
const changesKey = 'changes'
const allRows = rosterFolders.flatMap(folder => folder.rows.flatMap(row => [row, ...row.children]))
const rowFor = (ref: string) => allRows.find(row => row.ref === ref)!
const isMac = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform)
const modKey = isMac ? '⌘' : 'Ctrl+'
/** Opens at 304px so the two-line SR-2 row with its metrics line has room; the row stays two lines at every width (RM1-Q1). */
const sidebarDefault = 304
const changesDefault = 440
const narrowQuery = '(max-width: 639px)'
const subscribeNarrow = (notify: () => void) => {
  const media = window.matchMedia(narrowQuery)
  media.addEventListener('change', notify)
  return () => media.removeEventListener('change', notify)
}

function threadSync(phase: Phase): SyncStatus {
  if (phase === 'loading' || phase === 'thread-retry') return { _tag: 'Connecting', attempt: 1, since: milestoneNow - 1200 }
  if (phase === 'stale') return { _tag: 'Stale', reason: { _tag: 'Reconnecting', attempt: 2, nextAt: milestoneNow + 4000, issue: 'Gateway connection interrupted' }, lastLiveAt: lastSeen.at }
  return { _tag: 'Live', since: milestoneNow - 30_000 }
}

/** Only the agent last seen mid-run gets the "Last seen" tail; settled threads have nothing in flight to freeze. */
function threadView(phase: Phase, key: string, onRetry: () => void): ThreadView {
  if (phase === 'loading' || phase === 'thread-retry') return { _tag: 'Loading' }
  if (phase === 'stale') return key === mainAgentRef ? { _tag: 'Stale', lastSeen } : { _tag: 'Live' }
  if (phase === 'thread-unavailable') return { _tag: 'Unavailable', reason: 'Conversation unavailable', detail: 'The gateway could not load it (HTTP 503). Your draft is unchanged.', onRetry }
  return { _tag: 'Live' }
}

/** What a tab shows for an agent: the exact row status, except while the view itself is unknown or last-known. */
function agentTab(ref: string, phase: Phase): PaneTab {
  const row = rowFor(ref)
  if (phase === 'loading' || phase === 'thread-retry') return { key: ref, title: row.title, status: 'pending', statusLabel: 'Connecting' }
  if (phase === 'thread-unavailable') return { key: ref, title: row.title, status: 'offline', statusLabel: 'Unavailable' }
  if (phase === 'stale' && ref === mainAgentRef) return { key: ref, title: row.title, status: 'stale', statusLabel: 'Last seen working' }
  return { key: ref, title: row.title, status: row.status, statusLabel: row.statusLabel }
}

const initialTabs = (state: MilestoneState): readonly string[] =>
  state === 'empty-roster' ? [] : state === 'empty-thread' ? [idleAgentRef] : state === 'slice' || state === 'navigation' ? [mainAgentRef, reviewerRef] : [mainAgentRef]

export function RedesignMilestone(props: RedesignMilestoneProps) {
  const [phase, setPhase] = React.useState<Phase>(props.state)
  const [scheme, setScheme] = React.useState(props.scheme)
  const narrow = React.useSyncExternalStore(subscribeNarrow, () => window.matchMedia(narrowQuery).matches, () => false)
  const [width, setWidth] = usePersistedSize(props.persistKey, sidebarDefault)
  const [changesWidth, setChangesWidth] = usePersistedSize(`${props.persistKey}.changes`, changesDefault)
  // Docked: collapsed hides the sidebar. Narrow: the sidebar is a modal drawer, open only when the roster is the content.
  const [collapsed, setCollapsed] = React.useState(() => window.matchMedia(narrowQuery).matches && props.state !== 'empty-roster' && props.state !== 'roster-error')
  const [query, setQuery] = React.useState(props.state === 'no-match' ? 'zz-nothing' : '')
  const [paletteOpen, setPaletteOpen] = React.useState(props.state === 'navigation')
  const [tabs, setTabs] = React.useState(() => initialTabs(props.state))
  const [activeKey, setActiveKey] = React.useState(() => initialTabs(props.state)[0] ?? '')
  const [changesOpen, setChangesOpen] = React.useState(props.state === 'slice' || props.state === 'navigation')
  const [revealRequest, setRevealRequest] = React.useState<DiffRevealRequest>()
  const [sent, setSent] = React.useState<Readonly<Record<string, readonly TranscriptTurn[]>>>({})
  const searchRef = React.useRef<HTMLInputElement>(null)
  const timer = React.useRef<number | undefined>(undefined)
  React.useEffect(() => () => window.clearTimeout(timer.current), [])

  // Each Retry walks the real recovery path for its own fault only: the failed surface loads, then shows live data.
  const recover = React.useCallback((through: Phase) => {
    setPhase(through)
    window.clearTimeout(timer.current)
    timer.current = window.setTimeout(() => setPhase('slice'), 900)
  }, [])
  const retryRoster = React.useCallback(() => recover('roster-retry'), [recover])
  const retryThread = React.useCallback(() => recover('thread-retry'), [recover])

  const openAgent = React.useCallback((row: Row) => {
    setTabs(current => current.includes(row.ref) ? current : [...current, row.ref])
    setActiveKey(row.ref)
    if (window.matchMedia(narrowQuery).matches) setCollapsed(true)
  }, [])
  // The kit has no tool-output pane yet, so a call's "Open" shows the change set beside the thread, at the call's file.
  const openTool = React.useCallback((call: WorkLogCall) => {
    setChangesOpen(true)
    if (window.matchMedia(narrowQuery).matches) { setTabs(current => current.includes(changesKey) ? current : [...current, changesKey]); setActiveKey(changesKey) }
    const path = call.changedPath ?? changedFiles.find(file => file.path === call.argsSummary)?.path
    if (path !== undefined) setRevealRequest(previous => ({ path, sequence: (previous?.sequence ?? 0) + 1 }))
  }, [])
  const focusFilter = React.useCallback(() => { setCollapsed(false); requestAnimationFrame(() => searchRef.current?.focus()) }, [])

  // Global chords: Mod+K palette, Mod+B sidebar, "/" filter. Single keys never fire while typing.
  React.useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const mod = isMac ? event.metaKey : event.ctrlKey
      const target = event.target as HTMLElement | null
      const typing = target !== null && (target.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(target.tagName))
      if (mod && event.key.toLowerCase() === 'k') { event.preventDefault(); setPaletteOpen(true) }
      else if (mod && event.key.toLowerCase() === 'b') { event.preventDefault(); setCollapsed(value => !value) }
      else if (event.key === '/' && !typing && !mod) { event.preventDefault(); focusFilter() }
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [focusFilter])

  const roster = rosterState[phase]
  const sync = threadSync(phase)
  const renderInlineReference = React.useCallback((path: string) => {
    if (path === reviewerCard.path) return <AgentResultCard tab={agentTab(reviewerRef, 'slice')} summary={reviewerCard.summary} refinement={props.glyphRefinement} onOpen={() => openAgent(rowFor(reviewerRef))} />
    const resource = proseResources[path]
    return resource === undefined ? undefined : <ResourceChipV1 resource={resource} file={resource.files[0]!} />
  }, [openAgent, props.glyphRefinement])

  // Narrow: one pane, so Changes folds in as a tab. Docked: Changes is a resizable panel beside the thread.
  const paneKeys = narrow && changesOpen && !tabs.includes(changesKey) ? [...tabs, changesKey] : narrow ? tabs : tabs.filter(key => key !== changesKey)
  const shownKey = paneKeys.includes(activeKey) ? activeKey : paneKeys[0]
  const paneTabs = paneKeys.map(key => key === changesKey ? { key, title: 'Changes' } : agentTab(key, phase))
  const sidebarHidden = narrow || collapsed
  const showSidebar = <HeaderToggle label="Show sidebar" icon="panel" shortcut="Meta+B Control+B" onPress={() => setCollapsed(false)} />

  // The panel header names the change set; the "Changes" surface tab beside it says what kind of view it is.
  const changes = (inPane: boolean) => <DiffPanel open title="feat/metric-track" width="100%" diff={changedFiles[0]!.diff} path={changedFiles[0]!.path} added={2} removed={1}
    currentTurnFiles={changedFiles} branchFiles={branchFiles} revealRequest={revealRequest} onClose={inPane ? undefined : () => setChangesOpen(false)} />
  const thread = (key: string) => {
    const turns = [...(phase === 'empty-thread' || key === idleAgentRef ? [] : key === mainAgentRef ? (phase === 'stale' ? staleMainTurns : mainTurns) : key === reviewerRef ? reviewerTurns : []), ...(sent[key] ?? [])]
    const view = threadView(phase, key, retryThread)
    const running = key === mainAgentRef && view._tag === 'Live'
    const send = (text: string) => setSent(current => {
      const list = current[key] ?? []
      const id = `local-${key}-${list.length + 1}`
      const at = new Date(milestoneNow).toISOString()
      return { ...current, [key]: [...list, { id, prompt: { _tag: 'Text', id: `${id}-prompt`, role: 'user', text, attachments: [], streaming: false, at }, items: [], work: { calls: [], durationMs: undefined, running: false, failed: false, interrupted: false } }] }
    })
    return <MilestoneThread key={key} title={rowFor(key).title} view={view} now={milestoneNow}
      runtime={{ messages: turns.flatMap(turn => [...(turn.prompt === undefined ? [] : [turn.prompt]), ...turn.items]), isRunning: running, onNew: async message => send(message.content.flatMap(part => part.type === 'text' ? [part.text] : []).join('\n')) }}
      transcript={{ turns, sync, now: milestoneNow, observedAt: phase === 'stale' ? lastSeen.at : milestoneNow, renderInlineReference, onOpenTool: openTool, viewportKey: `${props.persistKey}.${key}` }}
      composer={{ running, folder: 'fractal-web', branch: 'feat/metric-track', onSend: send, onSteer: send, draftKey: `${props.persistKey}.${key}` }} />
  }

  const sidebar = <MilestoneSidebar state={roster} folders={phase === 'empty-roster' ? [] : rosterFolders} now={milestoneNow} activeRef={shownKey} query={query} onQueryChange={setQuery} searchRef={searchRef}
    glyphRefinement={props.glyphRefinement} sectionNav={props.sectionNav} inboxCount={phase === 'empty-roster' ? 0 : inboxCount}
    onOpen={openAgent} onRetry={retryRoster} onOpenPalette={() => setPaletteOpen(true)} onCollapse={() => setCollapsed(true)} collapseLabel={narrow ? 'Close sidebar' : 'Hide sidebar'} modKey={modKey} />
  const commands: readonly PaletteCommand[] = [
    { id: 'toggle-sidebar', label: sidebarHidden ? 'Show sidebar' : 'Hide sidebar', shortcut: `${modKey}B`, run: () => setCollapsed(value => !value) },
    { id: 'filter', label: 'Filter agents', shortcut: '/', run: focusFilter },
    { id: 'changes', label: changesOpen ? 'Hide changes' : 'Show changes', run: () => setChangesOpen(value => !value) },
    { id: 'scheme', label: scheme === 'dark' ? 'Switch to light theme' : 'Switch to dark theme', run: () => setScheme(scheme === 'dark' ? 'light' : 'dark') },
  ]
  const agents = rosterFolders.flatMap(folder => folder.rows.flatMap(row => [{ row, folder: folder.label }, ...row.children.map(child => ({ row: child, folder: folder.label }))]))

  return <div data-milestone-state={phase} data-scheme={scheme} data-narrow={narrow || undefined} {...stylex.props(...baselineTheme, scheme === 'light' && lightTheme, styles.root, scheme === 'light' && styles.light)}>
    <ThemedPortal>
      {!narrow && !collapsed && <>
        <div style={{ width }} {...stylex.props(styles.sidebarSlot)}>{sidebar}</div>
        <ResizableSplit id={props.persistKey} value={width} min={geometryNumbers.sidebarMin} max={440} onChange={setWidth} onToggle={() => setCollapsed(true)} onReset={() => setWidth(sidebarDefault)} collapsed={false} label="Resize sidebar" />
      </>}
      {narrow && <ModalOverlay isDismissable isOpen={!collapsed} onOpenChange={open => setCollapsed(!open)} {...stylex.props(styles.scrim)}>
        <Modal {...stylex.props(styles.drawer)}><Dialog aria-label="Agents" {...stylex.props(styles.drawerDialog)}>{sidebar}</Dialog></Modal>
      </ModalOverlay>}
      <main aria-label="Workspace" {...stylex.props(styles.main)}>
        {phase === 'stale' && <ConnectionStatus state="reconnecting" text="Next try in 4s · attempt 2" />}
        {shownKey === undefined
          ? <div {...stylex.props(styles.emptyFrame)}>
            {sidebarHidden && <div {...stylex.props(styles.emptyBar)}>{showSidebar}</div>}
            <div data-milestone-empty-main {...stylex.props(styles.emptyMain)}>
              <p {...stylex.props(styles.emptyTitle)}>No agent open</p>
              <dl {...stylex.props(styles.hints)}>
                <dt {...stylex.props(styles.hintKey)}><kbd {...stylex.props(styles.kbd)}>{modKey}K</kbd></dt><dd {...stylex.props(styles.hintText)}>Open an agent or run a command</dd>
                {roster !== 'empty' && <><dt {...stylex.props(styles.hintKey)}><kbd {...stylex.props(styles.kbd)}>/</kbd></dt><dd {...stylex.props(styles.hintText)}>Filter agents</dd></>}
                <dt {...stylex.props(styles.hintKey)}><kbd {...stylex.props(styles.kbd)}>{modKey}B</kbd></dt><dd {...stylex.props(styles.hintText)}>Show or hide the sidebar</dd>
              </dl>
            </div>
          </div>
          : <div {...stylex.props(styles.panes)}>
            <MilestonePane tabs={paneTabs} activeKey={shownKey} onSelectTab={setActiveKey} glyphRefinement={props.glyphRefinement}
              leading={sidebarHidden ? showSidebar : undefined} actions={narrow ? undefined : <ChangesToggle isSelected={changesOpen} onChange={setChangesOpen} />}>
              {shownKey === changesKey ? changes(true) : thread(shownKey)}
            </MilestonePane>
            {!narrow && changesOpen && <>
              <ResizableSplit id={`${props.persistKey}.changes`} reverse value={changesWidth} min={320} max={720} onChange={setChangesWidth} onToggle={() => setChangesOpen(false)} onReset={() => setChangesWidth(changesDefault)} collapsed={false} label="Resize changes" />
              <div style={{ width: changesWidth }} {...stylex.props(styles.changesSlot)}>{changes(false)}</div>
            </>}
          </div>}
      </main>
      <MilestonePalette isOpen={paletteOpen} onOpenChange={setPaletteOpen} scope={props.paletteScope} agents={agents} commands={commands} glyphRefinement={props.glyphRefinement} onOpenAgent={openAgent} />
    </ThemedPortal>
  </div>
}

/**
 * Overlays (palette, drawer, hover card) portal into a host inside the themed root so they inherit the scheme. The host is
 * state, not a ref, so an overlay that is open on the first frame re-renders into it once it exists.
 */
function ThemedPortal({ children }: { readonly children: React.ReactNode }) {
  const [host, setHost] = React.useState<HTMLDivElement | null>(null)
  const getContainer = React.useCallback(() => host, [host])
  return <UNSAFE_PortalProvider getContainer={getContainer}>{children}<div ref={setHost} {...stylex.props(styles.portalHost)} /></UNSAFE_PortalProvider>
}

const styles = stylex.create({
  root: { position: 'relative', display: 'flex', height: '100dvh', minWidth: 0, overflow: 'hidden', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, colorScheme: 'dark' },
  light: { colorScheme: 'light' },
  sidebarSlot: { flexShrink: 0, minWidth: 0, height: '100%' },
  scrim: { position: 'fixed', inset: 0, zIndex: 30, display: 'flex', backgroundColor: surface.scrim },
  drawer: { width: `min(${sidebarDefault}px, calc(100vw - ${g.gutter40}))`, height: '100%', outline: 'none', boxShadow: elevation.dialog },
  drawerDialog: { height: '100%', outline: 'none' },
  main: { display: 'flex', flexDirection: 'column', flex: '1 1 0', minWidth: 0, minHeight: 0 },
  panes: { display: 'flex', flex: '1 1 0', minWidth: 0, minHeight: 0 },
  changesSlot: { flexShrink: 0, minWidth: 0, height: '100%', borderInlineStartWidth: g.hairline, borderInlineStartStyle: 'solid', borderInlineStartColor: border.border },
  emptyFrame: { flex: '1 1 0', display: 'flex', flexDirection: 'column', minHeight: 0 },
  emptyBar: { display: 'flex', alignItems: 'center', height: g.controlLg, flexShrink: 0, paddingInline: s.sm },
  emptyMain: { flex: '1 1 0', display: 'flex', flexDirection: 'column', alignItems: 'center', justifyContent: 'center', gap: s.lg, padding: s.xl, minHeight: 0 },
  emptyTitle: { margin: 0, color: ink.fgSoft, fontSize: t.uiSize, lineHeight: t.uiLeading, fontWeight: t.weightMedium },
  hints: { display: 'grid', gridTemplateColumns: 'max-content max-content', columnGap: s.lg, rowGap: s.sm, margin: 0, alignItems: 'center' },
  hintKey: { margin: 0, justifySelf: 'end' },
  hintText: { margin: 0, color: ink.fgMuted },
  kbd: { fontFamily: t.fontSans, fontSize: t.denseSize, lineHeight: t.metaLeading, color: ink.fgSoft, paddingInline: s.xs, borderRadius: r.keyboard, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong },
  portalHost: { position: 'fixed', top: 0, left: 0, width: 0, height: 0, zIndex: 30 },
})
