import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { colorVars as c, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g } from '../composition-tokens.stylex'
import { surfaceOptions } from '../composition-model'

type DiffKind = 'context' | 'added' | 'removed' | 'hunk'
interface DiffRow { readonly kind: DiffKind; readonly text: string; readonly number?: number }

const rowsFromDiff = (diff: readonly string[]): readonly DiffRow[] => {
  const rows: DiffRow[] = []
  let added = 0
  let removed = 0
  for (const line of diff) {
    if (line.startsWith('@@')) { rows.push({ kind: 'hunk', text: line }); added = 0; removed = 0; continue }
    if (line.startsWith('+++') || line.startsWith('---')) continue
    if (line.startsWith('+')) rows.push({ kind: 'added', text: line.slice(1), number: ++added })
    else if (line.startsWith('-')) rows.push({ kind: 'removed', text: line.slice(1), number: ++removed })
    else rows.push({ kind: 'context', text: line.slice(1), number: ++added })
  }
  return rows
}

/** Collapses long context stretches into "N unmodified lines" rows. */
const withCollapsedContext = (rows: readonly DiffRow[]): readonly DiffRow[] => {
  const out: DiffRow[] = []
  let run: DiffRow[] = []
  const flush = () => {
    if (run.length >= 4) out.push({ kind: 'hunk', text: `${run.length} unmodified lines` })
    else out.push(...run)
    run = []
  }
  for (const row of rows) {
    if (row.kind === 'context') run.push(row)
    else { flush(); out.push(row) }
  }
  flush()
  return out
}

export function DiffPanel({ open, width, diff, path, added, removed, onOpenSurface }: {
  open: boolean
  width: number
  diff: readonly string[]
  path: string
  added: number
  removed: number
  onOpenSurface?: (key: string) => void
}) {
  const [tab, setTab] = React.useState<'diff' | 'preview' | 'usage'>('diff')
  if (!open) return null
  const rows = withCollapsedContext(rowsFromDiff(diff))
  return <aside aria-label="Right panel" data-testid="diff-panel" style={{ width }} {...stylex.props(styles.panel)}>
    <div role="tablist" aria-label="Surfaces" {...stylex.props(styles.tabs)}>
      {surfaceOptions.map(option => <button key={option.key} role="tab" type="button" aria-selected={tab === option.key} onClick={() => { setTab(option.key as typeof tab); onOpenSurface?.(option.key) }} {...stylex.props(styles.tab, tab === option.key && styles.tabOn)}>{option.label}</button>)}
    </div>
    {tab === 'diff' ? <div {...stylex.props(styles.body)}>
      <div {...stylex.props(styles.fileHead)}>
        <span {...stylex.props(styles.filePath)}>{path}</span>
        <span {...stylex.props(styles.fileAdded)}>+{added}</span>
        <span {...stylex.props(styles.fileRemoved)}>−{removed}</span>
        <button type="button" aria-label="Copy file path" title="Copy file path" onClick={() => void navigator.clipboard.writeText(path)} {...stylex.props(styles.copy)}>⧉</button>
      </div>
      <div role="region" aria-label="Diff lines" tabIndex={0} {...stylex.props(styles.code)} data-testid="diff-code">
        {rows.map((row, index) => row.kind === 'hunk'
          ? <div key={index} {...stylex.props(styles.hunkRow)}>{row.text}</div>
          : <div key={index} {...stylex.props(styles.codeRow, row.kind === 'added' && styles.codeAdded, row.kind === 'removed' && styles.codeRemoved)}>
              <span {...stylex.props(styles.gutter, row.kind === 'added' && styles.gutterAdded, row.kind === 'removed' && styles.gutterRemoved)}>{row.number ?? ''}</span>
              <span {...stylex.props(styles.sign, row.kind === 'added' && styles.signAdded, row.kind === 'removed' && styles.signRemoved)}>{row.kind === 'added' ? '+' : row.kind === 'removed' ? '−' : ' '}</span>
              <code {...stylex.props(styles.codeText)}>{row.text}</code>
            </div>)}
      </div>
    </div> : <div {...stylex.props(styles.emptySurface)}>
      <p {...stylex.props(styles.emptySurfaceTitle)}>Open a surface</p>
      {surfaceOptions.map(option => <button key={option.key} type="button" onClick={() => setTab(option.key as typeof tab)} {...stylex.props(styles.surfaceRow)}>
        <span>{option.label}</span>
        <kbd {...stylex.props(styles.kbd)}>{option.hint}</kbd>
      </button>)}
    </div>}
  </aside>
}

const styles = stylex.create({
  panel: { flexShrink: 0, height: '100%', backgroundColor: c.canvas, borderLeftWidth: 1, borderLeftStyle: 'solid', borderLeftColor: c.borderStrong, display: 'flex', flexDirection: 'column', minHeight: 0, overflow: 'hidden' },
  tabs: { height: g.band, display: 'flex', alignItems: 'center', gap: s.xs, paddingInline: s.lg, flexShrink: 0 },
  tab: { height: g.controlSm, maxWidth: 144, display: 'inline-flex', alignItems: 'center', paddingInline: s.md, borderWidth: 0, backgroundColor: 'transparent', color: c.fgMuted, borderRadius: r.control, fontSize: t.metaSize, lineHeight: t.metaLeading, cursor: 'pointer', ':hover': { color: c.fg }, ':focus-visible': { outline: `2px solid ${c.primary}`, outlineOffset: 1 } },
  tabOn: { backgroundColor: c.rowActive, color: c.fg },
  body: { display: 'flex', flexDirection: 'column', minHeight: 0, flexGrow: 1 },
  fileHead: { minHeight: 32, display: 'flex', alignItems: 'center', gap: s.md, paddingInline: s.lg, flexShrink: 0 },
  filePath: { fontSize: t.uiSize, lineHeight: t.uiLeading, color: c.fg },
  fileAdded: { fontFamily: t.fontMono, fontSize: t.denseSize, color: c.diffAdded, fontVariantNumeric: 'tabular-nums' },
  fileRemoved: { fontFamily: t.fontMono, fontSize: t.denseSize, color: c.diffRemoved, fontVariantNumeric: 'tabular-nums' },
  copy: { marginLeft: 'auto', width: g.controlSm, height: g.controlSm, display: 'inline-flex', alignItems: 'center', justifyContent: 'center', borderWidth: 0, backgroundColor: 'transparent', color: c.fgMuted, borderRadius: r.control, cursor: 'pointer', ':hover': { color: c.fg }, ':focus-visible': { outline: `2px solid ${c.primary}`, outlineOffset: 1 } },
  code: { overflow: 'auto', fontFamily: t.fontMono, fontSize: t.codeSize, lineHeight: '20px', paddingBottom: s.lg },
  codeRow: { display: 'flex', alignItems: 'stretch', whiteSpace: 'pre' },
  codeAdded: { backgroundColor: c.diffAddedWash },
  codeRemoved: { backgroundColor: c.diffRemovedWash },
  gutter: { width: 40, minWidth: 40, textAlign: 'right', paddingInline: s.sm, color: c.fgFaint, fontVariantNumeric: 'tabular-nums', userSelect: 'none' },
  gutterAdded: { backgroundColor: 'rgba(255, 255, 255, 0.02)', boxShadow: `inset 2px 0 0 0 ${c.diffAdded}` },
  gutterRemoved: { backgroundColor: 'rgba(255, 255, 255, 0.02)', boxShadow: `inset 2px 0 0 0 ${c.diffRemoved}` },
  sign: { width: 16, textAlign: 'center', userSelect: 'none', color: c.fgFaint },
  signAdded: { color: c.diffAdded },
  signRemoved: { color: c.diffRemoved },
  codeText: { fontFamily: 'inherit', fontSize: 'inherit', lineHeight: 'inherit', paddingInlineEnd: s.lg, color: c.fgSoft },
  hunkRow: { height: g.toolRow, display: 'flex', alignItems: 'center', justifyContent: 'center', fontSize: t.denseSize, color: c.fgFaint },
  emptySurface: { display: 'flex', flexDirection: 'column', alignItems: 'flex-start', gap: s.sm, padding: s.xl, flexGrow: 1 },
  emptySurfaceTitle: { margin: 0, marginBottom: s.sm, fontSize: t.uiSize, lineHeight: t.uiLeading, fontWeight: t.weightMedium, color: c.fg },
  surfaceRow: { display: 'flex', alignItems: 'center', gap: s.md, paddingInline: s.sm, paddingBlock: s.xs, borderWidth: 0, backgroundColor: 'transparent', color: c.fgSoft, fontSize: t.uiSize, lineHeight: t.uiLeading, borderRadius: r.control, cursor: 'pointer', width: '100%', ':hover': { backgroundColor: c.rowHover }, ':focus-visible': { outline: `2px solid ${c.primary}`, outlineOffset: 1 } },
  kbd: { marginLeft: 'auto', fontSize: t.metaSize, lineHeight: t.metaLeading, color: c.fgMuted, borderWidth: 1, borderStyle: 'solid', borderColor: c.borderStrong, borderRadius: 4, paddingInline: 6 },
})
