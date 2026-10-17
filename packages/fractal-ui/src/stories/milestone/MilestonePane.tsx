// Workshop-only composition. One 32px pane header owns identity: the title appears once, the run state appears once at the
// stream tail, and the kit Transcript renders without its own title band (`showHeader={false}`).
import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button, Tab, TabList, TabPanel, Tabs, ToggleButton } from 'react-aria-components'
import { Transcript, type TranscriptProps } from '../../assistant-ui/composition/Transcript'
import { Composer, type ComposerProps } from '../../assistant-ui/composition/Composer'
import { SystemNotice } from '../../assistant-ui/composition/SystemState'
import { TranscriptSkeleton } from '../../assistant-ui/composition/TranscriptSkeleton'
import { EmbraceRuntimeProvider } from '../../assistant-ui/EmbraceRuntime'
import { Icon } from '../../assistant-ui/composition/Icons'
import { SidebarStatus, type DiscRefinement } from '../../assistant-ui/sidebar/SidebarStatus'
import type { AgentStatus } from '../../assistant-ui/sidebar/model'
import type { ThreadResource } from '../../assistant-ui/workbench/workbench-model'
import { accentVars as accent, borderVars as border, geometryVars as g, radiusVars as r, spaceVars as s, statusVars as tone, surfaceVars as surface, textVars as ink, typeVars as t } from '../../assistant-ui/composition-tokens.stylex'

/** A pane tab: an agent conversation carries a status glyph; a tool surface (Changes) does not. */
export interface PaneTab { readonly key: string; readonly title: string; readonly status?: AgentStatus; readonly statusLabel?: string }

/** Workshop proposal: wide enough that a typical agent title is readable before it truncates. */
const tabMax = '240px'

/** Stale is an observation gap, not a failure: the glyph uses the muted tone, never the danger red. */
export const staleTone = stylex.createTheme(tone, { dangerFg: ink.fgMuted })

export function PaneGlyph({ tab, refinement }: { readonly tab: PaneTab; readonly refinement: DiscRefinement }) {
  if (tab.status === undefined) return null
  return <span {...stylex.props(styles.glyph, tab.status === 'stale' && staleTone)}><SidebarStatus status={tab.status} statusLabel={tab.statusLabel} variant="SG-1" discRefinement={refinement} iconOnly /></span>
}

export interface MilestonePaneProps {
  readonly tabs: readonly PaneTab[]
  readonly activeKey: string
  readonly onSelectTab: (key: string) => void
  readonly glyphRefinement: DiscRefinement
  /** Sidebar toggle while the sidebar is hidden (collapsed, or the closed narrow drawer). */
  readonly leading?: React.ReactNode
  readonly actions?: React.ReactNode
  readonly children: React.ReactNode
}

/** Same anatomy with one tab or many: a lone tab is the title, several are a tab strip; actions sit at the end. */
export function MilestonePane({ tabs, activeKey, onSelectTab, glyphRefinement, leading, actions, children }: MilestonePaneProps) {
  const active = tabs.find(tab => tab.key === activeKey) ?? tabs[0]!
  if (tabs.length === 1) return <section aria-label={active.title} data-milestone-pane={active.key} {...stylex.props(styles.pane)}>
    <header data-testid="milestone-pane-header" {...stylex.props(styles.header)}>
      {leading}
      <span {...stylex.props(styles.identity)}><PaneGlyph tab={active} refinement={glyphRefinement} /><span title={active.title} {...stylex.props(styles.title)}>{active.title}</span></span>
      <span {...stylex.props(styles.actions)}>{actions}</span>
    </header>
    <div {...stylex.props(styles.body)}>{children}</div>
  </section>
  return <Tabs selectedKey={active.key} onSelectionChange={key => onSelectTab(String(key))} data-milestone-pane={active.key} {...stylex.props(styles.pane)}>
    <header data-testid="milestone-pane-header" {...stylex.props(styles.header)}>
      {leading}
      <TabList aria-label="Open conversations" {...stylex.props(styles.tabs)}>
        {tabs.map(tab => <Tab key={tab.key} id={tab.key} {...stylex.props(styles.tab, tab.status === undefined && styles.toolTab)}><PaneGlyph tab={tab} refinement={glyphRefinement} /><span title={tab.title} {...stylex.props(styles.tabLabel)}>{tab.title}</span></Tab>)}
      </TabList>
      <span {...stylex.props(styles.actions)}>{actions}</span>
    </header>
    {/* One panel per tab with a stable id: re-using a single panel under a changing id leaves the selected tab's aria-controls pointing at nothing. */}
    {tabs.map(tab => <TabPanel key={tab.key} id={tab.key} {...stylex.props(styles.body)}>{tab.key === active.key ? children : null}</TabPanel>)}
  </Tabs>
}

export function HeaderToggle({ label, icon, shortcut, onPress }: { readonly label: string; readonly icon: 'panel' | 'chevrons-left'; readonly shortcut?: string; readonly onPress: () => void }) {
  return <Button aria-label={label} aria-keyshortcuts={shortcut} onPress={onPress} {...stylex.props(styles.iconButton)}><Icon name={icon} size={14} /></Button>
}
export function ChangesToggle({ isSelected, onChange }: { readonly isSelected: boolean; readonly onChange: (selected: boolean) => void }) {
  return <ToggleButton aria-label="Show changes" isSelected={isSelected} onChange={onChange} {...stylex.props(styles.iconButton, styles.toggle)}><Icon name="panel" size={14} /></ToggleButton>
}

export type ThreadView =
  | { readonly _tag: 'Live' }
  | { readonly _tag: 'Loading' }
  | { readonly _tag: 'Stale'; readonly lastSeen: { readonly activity: string; readonly at: number; readonly detail: string } }
  | { readonly _tag: 'Unavailable'; readonly reason: string; readonly detail: string; readonly onRetry: () => void }

export interface MilestoneThreadProps {
  readonly title: string
  readonly runtime: ThreadResource['runtime']
  readonly transcript: Omit<TranscriptProps, 'title' | 'showHeader'>
  readonly composer: Omit<ComposerProps, 'agent'>
  readonly view: ThreadView
  readonly now: number
}

/** The kit thread surface without its title band; the composer dock and lane match the kit pane exactly. */
export function MilestoneThread({ title, runtime, transcript, composer, view, now }: MilestoneThreadProps) {
  return <EmbraceRuntimeProvider options={runtime}>
    <div {...stylex.props(styles.frame)}>
      {view._tag === 'Unavailable' && <div data-thread-unavailable {...stylex.props(styles.lane, styles.notice)}><SystemNotice kind="error" title={view.reason} detail={view.detail} onRetry={view.onRetry} /></div>}
      {view._tag === 'Loading' && <div data-thread-loading {...stylex.props(styles.lane, styles.notice)}><TranscriptSkeleton sync={transcript.sync} now={now} observedAt={transcript.observedAt} /></div>}
      {(view._tag === 'Live' || view._tag === 'Stale') && <Transcript {...transcript} title={title} showHeader={false} />}
      {view._tag === 'Stale' && <LastSeen activity={view.lastSeen.activity} at={view.lastSeen.at} detail={view.lastSeen.detail} now={now} />}
    </div>
    <div data-testid="workbench-composer-dock" {...stylex.props(styles.dock, dockTheme)}>
      <div {...stylex.props(styles.lane)}><Composer agent={title} {...composer} /></div>
    </div>
  </EmbraceRuntimeProvider>
}

/** q23: last-known activity is labelled as last seen, with no spinner and no running clock. */
function LastSeen({ activity, at, detail, now }: { readonly activity: string; readonly at: number; readonly detail: string; readonly now: number }) {
  return <div {...stylex.props(styles.lane)}>
    <p role="status" data-last-seen {...stylex.props(styles.lastSeen)}>
      <span {...stylex.props(styles.glyph, staleTone)}><SidebarStatus status="stale" statusLabel="Last seen" variant="SG-1" discRefinement="G3" iconOnly /></span>
      <span>{`Last seen ${activity} · ${Math.round((now - at) / 1000)} s ago`}</span>
      <span aria-hidden="true" {...stylex.props(styles.sep)}>·</span>
      <code {...stylex.props(styles.code)}>{detail}</code>
    </p>
  </div>
}

/** RC-1 inside prose: phrasing content only (span + button), so the card is valid wherever the paragraph puts it. */
export function AgentResultCard({ tab, summary, refinement, onOpen }: { readonly tab: PaneTab; readonly summary: string; readonly refinement: DiscRefinement; readonly onOpen: () => void }) {
  return <span data-resource-card="RC-1" {...stylex.props(styles.card)}>
    <Button onPress={onOpen} {...stylex.props(styles.cardButton)}>
      <PaneGlyph tab={tab} refinement={refinement} />
      <span {...stylex.props(styles.cardTitle)}>{tab.title}</span>
      <span {...stylex.props(styles.cardSummary)}>{summary}</span>
      <span {...stylex.props(styles.cardMeta)}>{tab.statusLabel}</span>
      <span {...stylex.props(styles.cardMeta)}>Open</span>
    </Button>
  </span>
}

const dockTheme = stylex.createTheme(g, { blur: '0px' })
const focusRing = { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: '-2px' } as const
const styles = stylex.create({
  pane: { display: 'flex', flexDirection: 'column', flex: '1 1 0', minWidth: 0, minHeight: 0 },
  header: { display: 'flex', alignItems: 'center', gap: s.xs, height: g.controlLg, flexShrink: 0, boxSizing: 'border-box', paddingInline: s.sm, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border, minWidth: 0 },
  identity: { display: 'flex', alignItems: 'center', gap: s.sm, minWidth: 0, flex: '0 1 auto', paddingInline: s.sm },
  title: { minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', fontSize: t.metaSize, fontWeight: t.weightMedium, color: ink.fg },
  tabs: { display: 'flex', alignItems: 'center', gap: s.xs2, minWidth: 0, flex: '0 1 auto' },
  tab: { display: 'inline-flex', alignItems: 'center', gap: s.sm, height: g.controlSm, minWidth: 0, maxWidth: tabMax, paddingInline: s.sm, borderRadius: r.control, color: ink.fgMuted, fontSize: t.metaSize, cursor: 'pointer', outline: 'none', ':hover': { color: ink.fg }, ':is([data-selected])': { backgroundColor: surface.rowActive, color: ink.fg }, ':is([data-focus-visible])': focusRing },
  tabLabel: { minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  // Tool tabs (Changes) have a short fixed label; agent titles give way first when the strip is tight.
  toolTab: { flexShrink: 0 },
  glyph: { display: 'inline-flex', flexShrink: 0 },
  actions: { display: 'flex', alignItems: 'center', gap: s.xs2, marginInlineStart: 'auto', flexShrink: 0 },
  iconButton: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', width: g.controlSm, height: g.controlSm, flexShrink: 0, padding: 0, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgMuted, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover, color: ink.fg }, ':focus-visible': focusRing },
  toggle: { ':is([data-selected])': { color: ink.fg, backgroundColor: surface.rowActive } },
  body: { display: 'flex', flexDirection: 'column', flex: '1 1 0', minWidth: 0, minHeight: 0, outline: 'none' },
  frame: { flex: '1 1 0', minHeight: 0, minWidth: 0, display: 'flex', flexDirection: 'column' },
  notice: { paddingBlock: s.lg },
  dock: { flexShrink: 0, paddingBlock: s.sm, backgroundColor: surface.canvas },
  lane: { width: '100%', maxWidth: g.lane, boxSizing: 'border-box', paddingInline: s.lg, marginInline: 'auto' },
  lastSeen: { display: 'flex', alignItems: 'center', gap: s.sm, minWidth: 0, height: g.toolRow, margin: 0, color: ink.fgMuted, fontSize: t.metaSize, whiteSpace: 'nowrap', overflow: 'hidden' },
  sep: { color: ink.fgMuted },
  code: { minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', fontFamily: t.fontMono, fontSize: t.denseSize },
  card: { display: 'flex', width: '100%', minWidth: 0, borderRadius: r.md, overflow: 'hidden', backgroundColor: surface.washSubtle },
  cardButton: { display: 'flex', alignItems: 'center', gap: s.md, width: '100%', minWidth: 0, height: g.resourceCard, paddingInline: s.lg, borderWidth: 0, backgroundColor: surface.transparent, color: ink.fgSoft, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, textAlign: 'start', cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': focusRing },
  cardTitle: { flexShrink: 0, maxWidth: '45%', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', color: ink.fg, fontWeight: t.weightMedium },
  cardSummary: { flex: '1 1 0', minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', color: ink.fgMuted },
  cardMeta: { flexShrink: 0, color: ink.fgMuted, fontSize: t.denseSize, whiteSpace: 'nowrap' },
})
