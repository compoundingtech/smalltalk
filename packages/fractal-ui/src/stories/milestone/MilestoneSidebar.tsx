// Workshop-only composition: not exported from the package. The roster is one React Aria Tree (folder → agent → subagent);
// rows are a milestone SR-2 composition with fixed trailing columns, built from the kit's status, time and hover-card parts.
import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button, Disclosure, DisclosurePanel, Input, Label, Popover, SearchField, Tree, TreeItem, TreeItemContent, type Key } from 'react-aria-components'
import { AgentHoverCard, Highlight, sidebarRowAccessibleName } from '../../assistant-ui/sidebar/SidebarAgentRow'
import { SidebarStatus, type DiscRefinement } from '../../assistant-ui/sidebar/SidebarStatus'
import { SidebarTime } from '../../assistant-ui/sidebar/SidebarTime'
import { SidebarDuration } from '../../assistant-ui/sidebar/SidebarDuration'
import type { SidebarAgentRow as Row } from '../../assistant-ui/sidebar/model'
import { Icon } from '../../assistant-ui/composition/Icons'
import { accentVars as accent, borderVars as border, elevationVars as elevation, geometryVars as g, radiusVars as r, spaceVars as s, statusVars as tone, surfaceVars as surface, textVars as ink, typeVars as t } from '../../assistant-ui/composition-tokens.stylex'
import type { RosterFolder } from './milestone-fixtures'
import { staleTone } from './MilestonePane'

export type RosterState = 'ready' | 'loading' | 'empty' | 'error' | 'stale'
export interface MilestoneSidebarProps {
  readonly state: RosterState
  readonly folders: readonly RosterFolder[]
  readonly now: number
  readonly activeRef?: string
  readonly query: string
  readonly onQueryChange: (query: string) => void
  readonly searchRef: React.RefObject<HTMLInputElement | null>
  readonly glyphRefinement: DiscRefinement
  readonly sectionNav: 'footer' | 'header'
  readonly inboxCount: number
  readonly onOpen: (row: Row) => void
  readonly onRetry: () => void
  readonly onOpenPalette: () => void
  readonly onCollapse: () => void
  readonly collapseLabel: string
  readonly modKey: string
}

const spend = Intl.NumberFormat('en', { style: 'currency', currency: 'USD', notation: 'compact', maximumSignificantDigits: 2 })
const tokens = Intl.NumberFormat('en', { notation: 'compact', maximumSignificantDigits: 2 })
const folderKey = (id: string) => `folder:${id}`
const matches = (row: Row, needle: string) => `${row.title} ${row.host} ${row.ref}`.toLowerCase().includes(needle)

export function MilestoneSidebar(props: MilestoneSidebarProps) {
  const { state, folders, query } = props
  const needle = query.trim().toLowerCase()
  // Filtering keeps a parent when a subagent matches, so a hit never loses its context.
  const visible = needle === '' ? folders : folders.map(folder => ({ ...folder, rows: folder.rows.flatMap(row => {
    const children = row.children.filter(child => matches(child, needle))
    return matches(row, needle) || children.length > 0 ? [{ ...row, children: matches(row, needle) ? row.children : children }] : []
  }) })).filter(folder => folder.rows.length > 0)
  const [expanded, setExpanded] = React.useState<ReadonlySet<Key>>(() => new Set(folders.map(folder => folderKey(folder.id))))
  const shownExpanded = needle === '' ? expanded : new Set<Key>(visible.flatMap(folder => [folderKey(folder.id), ...folder.rows.filter(row => row.children.length > 0).map(row => row.ref)]))
  const showRows = state === 'ready' || state === 'stale'
  const treeRef = React.useRef<HTMLDivElement>(null)
  const rows = React.useMemo(() => new Map(folders.flatMap(folder => folder.rows.flatMap(row => [[row.ref, row] as const, ...row.children.map(child => [child.ref, child] as const)]))), [folders])
  const hover = useHoverCard()
  const toggle = (key: Key) => setExpanded(current => { const next = new Set(current); if (next.has(key)) next.delete(key); else next.add(key); return next })
  const sections = <div data-section-nav={props.sectionNav} {...stylex.props(styles.sections, props.sectionNav === 'footer' ? styles.sectionsFooter : styles.sectionsHeader)}>
    {/* Workshop labels: these sections have their own stories; here they only show where the row sits. */}
    <span {...stylex.props(styles.section)}>Inbox{props.inboxCount > 0 && <span aria-label={`${props.inboxCount} unread`} {...stylex.props(styles.count)}>{props.inboxCount}</span>}</span>
    <span {...stylex.props(styles.section)}>Missions</span>
    <span {...stylex.props(styles.section)}>Resources</span>
  </div>
  return <aside aria-label="Agents" data-roster-state={state} {...stylex.props(styles.sidebar)}>
    <header {...stylex.props(styles.header)}>
      <strong {...stylex.props(styles.brand)}>Fractal</strong>
      <Button aria-label={`Open command palette ${props.modKey}K`} aria-keyshortcuts="Meta+K Control+K" onPress={props.onOpenPalette} {...stylex.props(styles.kbdButton)}><Icon name="search" size={12} /><kbd {...stylex.props(styles.kbd)}>{props.modKey}K</kbd></Button>
      <Button aria-label={props.collapseLabel} aria-keyshortcuts="Meta+B Control+B" onPress={props.onCollapse} {...stylex.props(styles.iconButton)}><Icon name="chevrons-left" size={14} /></Button>
    </header>
    <SearchField value={query} onChange={props.onQueryChange} isDisabled={!showRows} {...stylex.props(styles.search)}>
      <Label {...stylex.props(styles.visuallyHidden)}>Filter agents</Label>
      <Icon name="search" size={12} />
      <Input ref={props.searchRef} placeholder="Filter agents" aria-keyshortcuts="/" onKeyDown={event => {
        const first = event.key === 'ArrowDown' ? treeRef.current?.querySelector<HTMLElement>('[role="row"]') : undefined
        if (first) { event.preventDefault(); first.focus() }
      }} {...stylex.props(styles.searchInput)} />
      {query === '' ? <kbd aria-hidden="true" {...stylex.props(styles.kbd)}>/</kbd> : <Button aria-label="Clear filter" {...stylex.props(styles.clear)}><Icon name="x" size={12} /></Button>}
    </SearchField>
    {props.sectionNav === 'header' && sections}
    <div data-roster-list data-testid="agent-sidebar" {...stylex.props(styles.list)}>
      {state === 'stale' && <p role="status" data-roster-notice="stale" {...stylex.props(styles.notice)}><Icon name="clock" size={12} />Last observed list</p>}
      {state === 'loading' && <>
        <p role="status" data-roster-notice="loading" {...stylex.props(styles.visuallyHidden)}>Loading agents…</p>
        <div aria-hidden="true" data-roster-skeleton>{[0.72, 0.54, 0.64, 0.46, 0.6, 0.5].map((share, index) => <div key={index} {...stylex.props(styles.skeletonRow)}><span {...stylex.props(styles.skeletonDisc)} /><span {...stylex.props(styles.skeletonLines)}><span {...stylex.props(styles.skeletonLine)} style={{ width: `${share * 100}%` }} /><span {...stylex.props(styles.skeletonLine, styles.skeletonMeta)} style={{ width: `${(share - 0.18) * 100}%` }} /></span></div>)}</div>
      </>}
      {state === 'empty' && <div data-roster-empty {...stylex.props(styles.message)}><p {...stylex.props(styles.messageTitle)}>No agents yet</p><p {...stylex.props(styles.messageBody)}>Agents appear here as soon as one starts.</p></div>}
      {state === 'error' && <div role="alert" data-roster-error {...stylex.props(styles.message, styles.messageStart)}>
        <p {...stylex.props(styles.messageTitle, styles.errorTitle)}><Icon name="alert" size={14} />Couldn’t load agents</p>
        <p {...stylex.props(styles.messageBody)}>The agent list did not load. Open conversations keep working.</p>
        <div {...stylex.props(styles.actions)}><Button onPress={props.onRetry} {...stylex.props(styles.actionButton)}>Retry</Button>
          <Disclosure {...stylex.props(styles.disclosure)}><Button slot="trigger" {...stylex.props(styles.linkButton)}>Details</Button><DisclosurePanel><p data-roster-error-detail {...stylex.props(styles.detail)}>roster read failed · HTTP 503 · attempt 3 of 3</p></DisclosurePanel></Disclosure>
        </div>
      </div>}
      {showRows && visible.length === 0 && <div data-roster-no-match {...stylex.props(styles.message)}><p {...stylex.props(styles.messageTitle)}>No agents match “{query.trim()}”</p><Button onPress={() => props.onQueryChange('')} {...stylex.props(styles.actionButton)}>Clear filter</Button></div>}
      {showRows && visible.length > 0 && <div onKeyDownCapture={event => {
        // ArrowUp from the first row returns to the filter: the list and its filter are one keyboard surface.
        if (event.key === 'ArrowUp' && event.target === treeRef.current?.querySelector('[role="row"]')) { event.preventDefault(); event.stopPropagation(); props.searchRef.current?.focus() }
      }}>
        <Tree ref={treeRef} aria-label="Agents" expandedKeys={shownExpanded} onExpandedChange={keys => { if (needle === '') setExpanded(new Set(keys)) }}
          onAction={key => { const row = rows.get(String(key)); if (row !== undefined) props.onOpen(row); else if (needle === '') toggle(key) }} {...stylex.props(styles.tree)}>
          {visible.map(folder => <TreeItem key={folder.id} id={folderKey(folder.id)} textValue={folder.label} {...stylex.props(styles.item)}>
            <TreeItemContent>{({ isExpanded, isFocusVisible }) => <div data-folder={folder.id} {...stylex.props(styles.folder, isFocusVisible && styles.focused)}>
              <Button slot="chevron" excludeFromTabOrder {...stylex.props(styles.chevron)}><Icon name={isExpanded ? 'chevron-down' : 'chevron-right'} size={12} /></Button>
              <span {...stylex.props(styles.folderLabel)}>{folder.label}</span><span aria-hidden="true" {...stylex.props(styles.count)}>{folder.rows.length}</span>
            </div>}</TreeItemContent>
            {folder.rows.map(row => <AgentItem key={row.ref} row={row} {...props} hover={hover} />)}
          </TreeItem>)}
        </Tree>
      </div>}
    </div>
    {props.sectionNav === 'footer' && sections}
    <Popover isNonModal isOpen={hover.row !== undefined} onOpenChange={open => { if (!open) hover.close() }} triggerRef={hover.anchor} placement="right top" offset={8} {...stylex.props(styles.hovercard)}>
      {hover.row !== undefined && <AgentHoverCard row={hover.row} now={props.now} />}
    </Popover>
  </aside>
}

interface HoverCard {
  readonly row: Row | undefined
  readonly anchor: React.RefObject<HTMLElement | null>
  readonly start: (row: Row, element: HTMLElement | null) => void
  readonly end: () => void
  readonly close: () => void
}

/** Pointer hover opens the kit hover card after 150ms beside the row; leaving closes it after 300ms. */
function useHoverCard(): HoverCard {
  const [row, setRow] = React.useState<Row>()
  const anchor = React.useRef<HTMLElement | null>(null)
  const timer = React.useRef<number | undefined>(undefined)
  React.useEffect(() => () => window.clearTimeout(timer.current), [])
  return React.useMemo(() => ({
    row, anchor,
    start: (next: Row, element: HTMLElement | null) => { window.clearTimeout(timer.current); timer.current = window.setTimeout(() => { anchor.current = element; setRow(next) }, 150) },
    end: () => { window.clearTimeout(timer.current); timer.current = window.setTimeout(() => setRow(undefined), 300) },
    close: () => { window.clearTimeout(timer.current); setRow(undefined) },
  }), [row])
}

type AgentItemProps = MilestoneSidebarProps & { readonly row: Row; readonly hover: HoverCard }
function AgentItem({ row, hover, ...props }: AgentItemProps) {
  const element = React.useRef<HTMLDivElement>(null)
  const active = row.ref === props.activeRef
  return <TreeItem ref={element} id={row.ref} textValue={row.title} aria-label={`${sidebarRowAccessibleName(row)}${active ? ', open' : ''}`}
    onHoverStart={() => hover.start(row, element.current)} onHoverEnd={hover.end} {...stylex.props(styles.item)}>
    <TreeItemContent>{({ hasChildItems, isExpanded, isFocusVisible, level }) => <MilestoneAgentRow row={row} level={level} active={active} focused={isFocusVisible} query={props.query} now={props.now} glyphRefinement={props.glyphRefinement} stale={props.state === 'stale'}
      chevron={hasChildItems ? <Button slot="chevron" excludeFromTabOrder aria-label={`${isExpanded ? 'Collapse' : 'Expand'} ${row.children.length} subagents`} {...stylex.props(styles.chevron)}><Icon name={isExpanded ? 'chevron-down' : 'chevron-right'} size={12} /></Button> : null} />}</TreeItemContent>
    {row.children.map(child => <AgentItem key={child.ref} {...props} row={child} hover={hover} />)}
  </TreeItem>
}

/**
 * SR-2 two-line row. Line 1: glyph · title · last turn · signal · chevron, each trailing column a fixed width so every
 * time ends on one edge and the title ellipsis keeps a 6px gap. Line 2: status word (unless working) · host · spend · duration.
 */
export function MilestoneAgentRow({ row, level, active, focused, query, now, glyphRefinement, stale, chevron }: { readonly row: Row; readonly level: number; readonly active: boolean; readonly focused: boolean; readonly query: string; readonly now: number; readonly glyphRefinement: DiscRefinement; readonly stale: boolean; readonly chevron: React.ReactNode }) {
  const fields: React.ReactNode[] = []
  if (row.status !== 'working') fields.push(<span key="status" data-row-field="status-label">{row.statusLabel}</span>)
  fields.push(<span key="host" data-row-field="host">{row.host}</span>)
  if (row.usage._tag === 'Known') fields.push(<span key="spend" data-row-field="spend" aria-label={`Spend including subagents: $${row.usage.usd}, ${row.usage.tokens} tokens`}>{spend.format(row.usage.usd)} · {tokens.format(row.usage.tokens)}</span>)
  if (row.duration._tag === 'Known') fields.push(<span key="duration" data-row-field="duration"><SidebarDuration duration={row.duration} /></span>)
  return <div data-testid="milestone-agent-row" data-status={row.status} data-level={level} {...stylex.props(styles.row, level > 2 && styles.nested, active && styles.active, focused && styles.focused)}>
    <span data-row-column="glyph" {...stylex.props(styles.glyph, row.status === 'stale' && staleTone)}><SidebarStatus status={row.status} statusLabel={row.statusLabel} variant="SG-1" discRefinement={glyphRefinement} freshness={stale ? 'stale' : row.freshness} iconOnly /></span>
    <span data-row-column="title" {...stylex.props(styles.title)}><Highlight value={row.title} query={query} /></span>
    <span data-row-column="time" {...stylex.props(styles.time)}>{row.lastTurn._tag === 'Known' && <SidebarTime at={row.lastTurn.at} now={now} kind={row.lastTurn.kind === 'turn-completed' ? 'turn' : 'activity'} compact />}</span>
    <span data-row-column="signal" {...stylex.props(styles.signal)}>{row.needsMe ? <span aria-label="Needs your attention" {...stylex.props(styles.needs)}>!</span> : (row.unread ?? 0) > 0 ? <span aria-label={`${row.unread} unread`} {...stylex.props(styles.unread)}>{row.unread}</span> : null}</span>
    <span data-row-column="chevron" {...stylex.props(styles.chevronSlot)}>{chevron}</span>
    <span data-row-column="line2" {...stylex.props(styles.line2)}>{fields.flatMap((field, index) => index === 0 ? [field] : [<span key={`sep-${index}`} aria-hidden="true" {...stylex.props(styles.sep)}>·</span>, field])}</span>
  </div>
}

const focusRing = { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: '-2px' } as const
const styles = stylex.create({
  sidebar: { display: 'flex', flexDirection: 'column', height: '100%', minWidth: 0, minHeight: 0, backgroundColor: surface.sidebar, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading },
  header: { display: 'flex', alignItems: 'center', gap: s.xs, height: g.controlLg, flexShrink: 0, paddingInline: s.md, paddingBlockStart: s.xs },
  brand: { flex: '1 1 0', minWidth: 0, paddingInlineStart: s.xs, fontSize: t.uiSize, lineHeight: t.uiLeading, fontWeight: t.weightSemibold },
  iconButton: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', width: g.controlSm, height: g.controlSm, padding: 0, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.sidebarFgMuted, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover, color: ink.fg }, ':focus-visible': focusRing },
  kbdButton: { display: 'inline-flex', alignItems: 'center', gap: s.xs, height: g.controlSm, paddingInline: s.sm, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.sidebarFgMuted, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover, color: ink.fg }, ':focus-visible': focusRing },
  kbd: { fontFamily: t.fontSans, fontSize: t.denseSize, lineHeight: t.metaLeading, color: ink.fgMuted, paddingInline: s.xs, borderRadius: r.keyboard, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong },
  search: { display: 'flex', alignItems: 'center', gap: s.sm, height: g.controlMd, flexShrink: 0, marginInline: s.md, marginBlock: s.xs, paddingInline: s.sm, boxSizing: 'border-box', borderRadius: r.sm, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border, backgroundColor: surface.controlFill, color: ink.fgMuted, ':focus-within': { borderColor: accent.primary } },
  searchInput: { flex: '1 1 0', minWidth: 0, padding: 0, borderWidth: 0, backgroundColor: surface.transparent, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, outline: 'none', '::placeholder': { color: ink.fgMuted }, '::-webkit-search-cancel-button': { display: 'none' } },
  clear: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', width: g.icon, height: g.icon, padding: 0, borderWidth: 0, borderRadius: r.keyboard, backgroundColor: surface.transparent, color: ink.fgMuted, cursor: 'pointer', ':focus-visible': focusRing },
  list: { flex: '1 1 0', minHeight: 0, overflowY: 'auto', scrollbarGutter: 'stable', scrollbarWidth: 'thin', scrollbarColor: `${tone.scrollThumb} transparent`, paddingInline: s.xs, paddingBlockEnd: s.md, display: 'flex', flexDirection: 'column', gap: s.xs2 },
  tree: { display: 'flex', flexDirection: 'column', gap: s.xs2, outline: 'none' },
  item: { outline: 'none', cursor: 'pointer' },
  folder: { display: 'flex', alignItems: 'center', gap: s.xs, height: g.controlSm, marginBlockStart: s.sm, paddingInlineEnd: s.sm, boxSizing: 'border-box', borderRadius: r.sm, color: ink.sidebarFgMuted, fontSize: t.denseSize, fontWeight: t.weightMedium, ':hover': { color: ink.fg } },
  folderLabel: { flex: '1 1 0', minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  chevron: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', width: g.icon, height: g.icon, padding: 0, marginInlineStart: s.xs, borderWidth: 0, borderRadius: r.keyboard, backgroundColor: surface.transparent, color: ink.sidebarFgMuted, cursor: 'pointer', ':hover': { color: ink.fg, backgroundColor: surface.rowActive } },
  count: { color: ink.fgMuted, fontSize: t.denseSize, fontVariantNumeric: 'tabular-nums' },
  row: { display: 'grid', gridTemplateColumns: `${g.icon} minmax(0, 1fr) ${g.sidebarMetric} ${g.status} ${g.icon}`, gridTemplateRows: `${g.controlSm} ${g.controlSm}`, alignItems: 'center', columnGap: s.sm, height: g.sidebarRow, boxSizing: 'border-box', paddingInline: s.sm, borderRadius: r.sm, color: ink.fg, ':hover': { backgroundColor: surface.rowHover } },
  nested: { paddingInlineStart: s.xl },
  active: { backgroundColor: surface.rowActive },
  focused: focusRing,
  glyph: { gridColumn: '1', gridRow: '1', display: 'flex', alignItems: 'center', justifyContent: 'center' },
  title: { gridColumn: '2', gridRow: '1', minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', fontSize: t.uiSize, lineHeight: t.uiLeading, fontWeight: t.weightMedium },
  time: { gridColumn: '3', gridRow: '1', justifySelf: 'end', color: ink.fgSoft, fontSize: t.denseSize, fontVariantNumeric: 'tabular-nums', whiteSpace: 'nowrap' },
  signal: { gridColumn: '4', gridRow: '1', display: 'flex', justifyContent: 'center' },
  needs: { color: tone.attention, fontSize: t.denseSize, fontWeight: t.weightSemibold },
  unread: { color: accent.primary, fontSize: t.microSize, fontVariantNumeric: 'tabular-nums' },
  chevronSlot: { gridColumn: '5', gridRow: '1', display: 'flex', justifyContent: 'center' },
  line2: { gridColumn: '2 / -1', gridRow: '2', minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', color: ink.fgSoft, fontSize: t.denseSize, fontVariantNumeric: 'tabular-nums' },
  sep: { paddingInline: s.xs, color: ink.fgMuted },
  notice: { display: 'flex', alignItems: 'center', gap: s.sm, minHeight: g.controlSm, margin: 0, paddingInline: s.sm, color: ink.fgMuted, fontSize: t.denseSize },
  message: { display: 'flex', flexDirection: 'column', alignItems: 'flex-start', gap: s.xs, margin: 0, paddingInline: s.sm, paddingBlock: s.lg },
  messageStart: { paddingBlockStart: s.md },
  messageTitle: { display: 'flex', alignItems: 'center', gap: s.sm, margin: 0, color: ink.fg, fontSize: t.metaSize, fontWeight: t.weightMedium },
  errorTitle: { color: tone.dangerFg },
  messageBody: { margin: 0, color: ink.fgMuted, fontSize: t.denseSize, lineHeight: t.metaLeading },
  actions: { display: 'flex', alignItems: 'flex-start', gap: s.md, marginBlockStart: s.xs },
  disclosure: { display: 'flex', flexDirection: 'column', gap: s.xs },
  actionButton: { height: g.controlSm, paddingInline: s.md, borderRadius: r.sm, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, backgroundColor: surface.controlFill, color: ink.fg, fontFamily: t.fontSans, fontSize: t.denseSize, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': focusRing },
  linkButton: { height: g.controlSm, padding: 0, borderWidth: 0, backgroundColor: surface.transparent, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.denseSize, cursor: 'pointer', ':hover': { color: ink.fg }, ':focus-visible': focusRing },
  detail: { margin: 0, color: ink.fgMuted, fontFamily: t.fontMono, fontSize: t.denseSize, overflowWrap: 'anywhere' },
  skeletonRow: { display: 'grid', gridTemplateColumns: `${g.icon} minmax(0, 1fr)`, alignItems: 'center', columnGap: s.sm, height: g.sidebarRow, paddingInline: s.sm },
  skeletonDisc: { justifySelf: 'center', width: s.md, height: s.md, borderRadius: r.full, backgroundColor: surface.rowActive },
  skeletonLines: { display: 'flex', flexDirection: 'column', gap: s.md, minWidth: 0 },
  skeletonLine: { height: s.md, borderRadius: r.keyboard, backgroundColor: surface.rowActive },
  skeletonMeta: { height: s.sm, backgroundColor: surface.rowHover },
  sections: { display: 'flex', alignItems: 'center', gap: s.lg, flexShrink: 0, height: g.controlLg, paddingInline: s.lg, boxSizing: 'border-box', color: ink.sidebarFgMuted, fontSize: t.denseSize },
  sectionsFooter: { borderTopWidth: g.hairline, borderTopStyle: 'solid', borderTopColor: border.border },
  sectionsHeader: { borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border },
  section: { display: 'inline-flex', alignItems: 'center', gap: s.xs },
  hovercard: { backgroundColor: surface.raised, color: ink.fg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.md, padding: s.lg, boxSizing: 'border-box', fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, maxWidth: 'calc(100vw - 32px)', boxShadow: elevation.popover, outline: 'none' },
  visuallyHidden: { position: 'absolute', width: '1px', height: '1px', margin: '-1px', padding: 0, overflow: 'hidden', clip: 'rect(0 0 0 0)', whiteSpace: 'nowrap', borderWidth: 0 },
})
