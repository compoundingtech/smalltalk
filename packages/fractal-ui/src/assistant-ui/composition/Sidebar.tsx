import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { colorVars as c, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g } from '../composition-tokens.stylex'
import type { AgentRowData, AgentStatus, FolderGroupData } from '../composition-model'

const spin = stylex.keyframes({ to: { transform: 'rotate(360deg)' } })

/** One glyph per agent/run state; color lives on the glyph only. */
export function StatusGlyph({ status, elapsed }: { status: AgentStatus; elapsed?: string }) {
  const mark = status === 'done' ? '✓' : status === 'attention' ? '!' : status === 'failed' ? '✕' : status === 'input' ? '…' : ''
  const dot = status === 'working' ? styles.dotWorking : status === 'done' ? styles.dotDone : status === 'attention' ? styles.dotAttention : status === 'failed' ? styles.dotFailed : status === 'input' ? styles.dotInput : styles.dotIdle
  return <span data-status={status} title={status} {...stylex.props(styles.glyphWrap)}>
    <span {...stylex.props(styles.glyphDot, dot)}>
      {mark}
      {status === 'working' ? <span aria-hidden="true" {...stylex.props(styles.glyphRing)} /> : null}
    </span>
    {elapsed ? <span {...stylex.props(styles.glyphElapsed)}>{elapsed}</span> : null}
  </span>
}

export function AgentRow({ row, active, onSelect }: { row: AgentRowData; active: boolean; onSelect: () => void }) {
  return <button type="button" aria-current={active} onClick={onSelect} {...stylex.props(styles.row, active && styles.rowActive)}>
    <span {...stylex.props(styles.rowTitle)}>
      <span {...stylex.props(styles.rowTitleText, active && styles.rowTitleOn)}>{row.title}</span>
      <span {...stylex.props(styles.rowTime)}>{row.time}</span>
    </span>
    <span {...stylex.props(styles.rowMeta)}>
      <span>{row.agent}</span>
      <StatusGlyph status={row.status} />
    </span>
  </button>
}

export function FolderGroup({ group, activeId, onSelect }: { group: FolderGroupData; activeId: string; onSelect: (id: string) => void }) {
  const [open, setOpen] = React.useState(true)
  return <section {...stylex.props(styles.group)}>
    <button type="button" aria-expanded={open} onClick={() => setOpen(value => !value)} {...stylex.props(styles.groupHeader)}>
      <span {...stylex.props(styles.groupLabel)}>{group.label}</span>
      <span aria-hidden="true" {...stylex.props(styles.groupRule)} />
      <span aria-hidden="true" {...stylex.props(styles.groupChevron, open && styles.groupChevronOpen)}>▾</span>
    </button>
    {open ? <div {...stylex.props(styles.groupRows)}>{group.rows.map(row => <AgentRow key={row.id} row={row} active={row.id === activeId} onSelect={() => onSelect(row.id)} />)}</div> : null}
  </section>
}

export function AgentSidebar({ groups, activeId, onSelect, onCollapse, collapsed }: {
  groups: readonly FolderGroupData[]
  activeId: string
  onSelect: (id: string) => void
  onCollapse: () => void
  collapsed: boolean
}) {
  return <div data-testid="agent-sidebar" {...stylex.props(styles.sidebar, collapsed && styles.sidebarCollapsed)}>
    <div {...stylex.props(styles.brandRow)}>
      <button type="button" aria-label={collapsed ? 'Expand sidebar' : 'Collapse sidebar'} title={collapsed ? 'Expand sidebar' : 'Collapse sidebar'} onClick={onCollapse} {...stylex.props(styles.ghostMd)}>{collapsed ? '»' : '«'}</button>
      {collapsed ? null : <span {...stylex.props(styles.wordmark)}>Fractal<span {...stylex.props(styles.wordmarkMuted)}> workshop</span></span>}
    </div>
    {collapsed ? null : <>
      <div {...stylex.props(styles.searchRow)}>
        <span aria-hidden="true" {...stylex.props(styles.searchGlyph)}>⌕</span>
        <span {...stylex.props(styles.searchText)}>Search</span>
        <span {...stylex.props(styles.searchActions)}>
          <button type="button" aria-label="Add folder" title="Add folder" {...stylex.props(styles.ghostMd)}>⊕</button>
          <button type="button" aria-label="New thread" title="New thread" {...stylex.props(styles.ghostMd)}>✎</button>
        </span>
      </div>
      <nav aria-label="Folders and agents" {...stylex.props(styles.nav)}>
        {groups.map(group => <FolderGroup key={group.id} group={group} activeId={activeId} onSelect={onSelect} />)}
      </nav>
    </>}
    <div {...stylex.props(styles.footer)}>
      <button type="button" aria-label="Settings" title="Settings" {...stylex.props(styles.ghostLg)}>⚙</button>
      <button type="button" aria-label="Reviews" title="Reviews" {...stylex.props(styles.ghostLg)}>⇄</button>
      <button type="button" aria-label="Usage" title="Usage" {...stylex.props(styles.ghostLg)}>◷</button>
    </div>
  </div>
}

const styles = stylex.create({
  sidebar: { width: '100%', height: '100%', backgroundColor: c.sidebar, color: c.fg, display: 'flex', flexDirection: 'column', minHeight: 0, overflow: 'hidden' },
  sidebarCollapsed: { width: 0 },
  brandRow: { height: g.band, display: 'flex', alignItems: 'center', gap: s.lg, paddingInline: s.lg, flexShrink: 0 },
  wordmark: { fontSize: t.uiSize, fontWeight: t.weightMedium, letterSpacing: '-0.01em', whiteSpace: 'nowrap' },
  wordmarkMuted: { color: c.sidebarFgMuted, fontWeight: 400 },
  searchRow: { height: g.controlLg, display: 'flex', alignItems: 'center', gap: s.sm, paddingInline: s.lg, marginInline: s.md, borderRadius: r.md, color: c.sidebarFgMuted, flexShrink: 0 },
  searchGlyph: { fontSize: t.uiSize, width: 16, textAlign: 'center' },
  searchText: { fontSize: t.uiSize, lineHeight: t.uiLeading, marginInlineEnd: 'auto' },
  searchActions: { display: 'flex', gap: s.xs },
  nav: { display: 'flex', flexDirection: 'column', gap: s.xs, paddingInline: s.md, paddingBlock: s.sm, minHeight: 0, overflowY: 'auto', flexGrow: 1 },
  group: { display: 'flex', flexDirection: 'column' },
  groupHeader: { height: g.controlLg, display: 'flex', alignItems: 'center', gap: s.sm, paddingInline: s.md, borderWidth: 0, backgroundColor: 'transparent', cursor: 'pointer', minWidth: 0 },
  groupLabel: { fontSize: t.metaSize, lineHeight: t.metaLeading, fontWeight: t.weightMedium, color: c.sidebarFgMuted, whiteSpace: 'nowrap' },
  groupRule: { height: 1, flexGrow: 1, backgroundColor: c.border },
  groupChevron: { fontSize: t.metaSize, color: c.sidebarFgMuted, transform: 'rotate(-90deg)', transition: 'transform 150ms ease' },
  groupChevronOpen: { transform: 'none' },
  groupRows: { display: 'flex', flexDirection: 'column', gap: 2 },
  row: { display: 'flex', flexDirection: 'column', gap: 2, alignItems: 'stretch', textAlign: 'left', borderWidth: 0, backgroundColor: 'transparent', borderRadius: r.md, paddingBlock: s.sm, paddingInline: s.lg, cursor: 'pointer', color: c.fg, transition: 'background-color 150ms ease', ':hover': { backgroundColor: c.rowHover }, ':focus-visible': { outline: `2px solid ${c.primary}`, outlineOffset: -2 } },
  rowActive: { backgroundColor: c.rowActive },
  rowTitle: { display: 'flex', alignItems: 'baseline', gap: s.sm, minWidth: 0 },
  rowTitleText: { fontSize: t.uiSize, lineHeight: t.uiLeading, color: c.fgSoft, whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis', flexGrow: 1 },
  rowTitleOn: { color: c.fg, fontWeight: t.weightMedium },
  rowTime: { fontSize: t.metaSize, lineHeight: t.metaLeading, color: c.sidebarFgMuted, fontVariantNumeric: 'tabular-nums', flexShrink: 0 },
  rowMeta: { display: 'flex', alignItems: 'center', gap: s.sm, fontSize: t.metaSize, lineHeight: t.metaLeading, color: c.sidebarFgMuted },
  glyphWrap: { display: 'inline-flex', alignItems: 'center', gap: s.xs, marginLeft: 'auto', fontVariantNumeric: 'tabular-nums' },
  glyphDot: { width: 12, height: 12, borderRadius: '50%', display: 'inline-flex', alignItems: 'center', justifyContent: 'center', position: 'relative', fontSize: '8px', lineHeight: 1, color: c.fg },
  glyphRing: { position: 'absolute', inset: -2, borderRadius: '50%', borderWidth: 1, borderStyle: 'dashed', borderColor: c.running, animationName: spin, animationDuration: '1.2s', animationTimingFunction: 'linear', animationIterationCount: 'infinite' },
  glyphElapsed: { fontSize: t.metaSize, lineHeight: t.metaLeading, color: c.sidebarFgMuted },
  footer: { height: 40, display: 'flex', alignItems: 'center', gap: s.xs, paddingInline: s.lg, flexShrink: 0 },
  ghostMd: { width: 28, height: 28, display: 'inline-flex', alignItems: 'center', justifyContent: 'center', borderWidth: 0, backgroundColor: 'transparent', color: c.sidebarFgMuted, borderRadius: r.control, cursor: 'pointer', fontSize: t.uiSize, transition: 'background-color 150ms ease, color 150ms ease', ':hover': { backgroundColor: c.rowHover, color: c.fg }, ':focus-visible': { outline: `2px solid ${c.primary}`, outlineOffset: 1 } },
  ghostLg: { width: 32, height: 32, display: 'inline-flex', alignItems: 'center', justifyContent: 'center', borderWidth: 0, backgroundColor: 'transparent', color: c.sidebarFgMuted, borderRadius: r.control, cursor: 'pointer', fontSize: t.uiSize, transition: 'background-color 150ms ease, color 150ms ease', ':hover': { backgroundColor: c.rowHover, color: c.fg }, ':focus-visible': { outline: `2px solid ${c.primary}`, outlineOffset: 1 } },
  dotIdle: { backgroundColor: c.fgFaint },
  dotWorking: { backgroundColor: c.running },
  dotDone: { backgroundColor: c.done, color: c.sidebar },
  dotAttention: { backgroundColor: c.attention, color: c.sidebar },
  dotFailed: { backgroundColor: c.danger, color: c.sidebar },
  dotInput: { backgroundColor: c.runningFg, color: c.sidebar },
})
