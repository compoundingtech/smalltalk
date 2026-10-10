import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Tabs, TabList, Tab, TabPanel, Select, SelectValue, Button as AriaButton, ToggleButton, Popover, ListBox, ListBoxItem } from 'react-aria-components'
import { surfaceVars as surface, textVars as text, borderVars as border, accentVars as accent, statusVars as status, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g } from '../composition-tokens.stylex'
import { surfaceOptions } from '../composition-model'
import { Icon } from './Icons'
import { Button, IconButton, Tooltip } from './Controls'
import { HighlightedSource, languageFromPath } from './Markdown'
import { ThemePortal } from '../taste/ThemePortal'

type DiffKind = 'context' | 'added' | 'removed' | 'hunk'
type DiffScope = 'current' | 'branch'
interface DiffRow { readonly kind: DiffKind; readonly text: string; readonly oldNumber?: number; readonly newNumber?: number; readonly hidden?: readonly DiffRow[] }
export interface DiffFile { readonly path: string; readonly diff: readonly string[]; readonly added: number; readonly removed: number }
export interface DiffTurn extends DiffFile { readonly id: string; readonly label: string }
export interface DiffRevealRequest { readonly path: string; readonly sequence: number }
export interface DiffPanelProps {
  open: boolean
  width?: number | string
  title?: string
  /** Disambiguates landmarks in a same-page comparison; visual titles stay unchanged. */
  landmarkContext?: string
  diff?: readonly string[]
  path?: string
  added?: number
  removed?: number
  currentTurnFiles?: readonly DiffFile[]
  branchFiles?: readonly DiffFile[]
  revealRequest?: DiffRevealRequest
  onOpenSurface?: (key: string) => void
  onClose?: () => void
  turns?: readonly DiffTurn[]
}

function withoutWhitespaceChanges(rows: readonly DiffRow[]) {
  const visible: DiffRow[] = []
  let index = 0
  while (index < rows.length) {
    const row = rows[index]!
    if (row.kind !== 'added' && row.kind !== 'removed') { visible.push(row); index += 1; continue }
    const changes: DiffRow[] = []
    while (index < rows.length && (rows[index]!.kind === 'added' || rows[index]!.kind === 'removed')) changes.push(rows[index++]!)
    const removed = changes.filter(line => line.kind === 'removed')
    const added = changes.filter(line => line.kind === 'added')
    const hidden = new Set<DiffRow>()
    for (let pair = 0; pair < Math.min(removed.length, added.length); pair += 1) {
      if (removed[pair]!.text.replace(/\s/g, '') === added[pair]!.text.replace(/\s/g, '')) {
        hidden.add(removed[pair]!)
        hidden.add(added[pair]!)
      }
    }
    for (const change of changes) if (!hidden.has(change) && change.text.trim() !== '') visible.push(change)
  }
  return visible
}

function rowsFromDiff(diff: readonly string[], hideWhitespace: boolean) {
  const rows: DiffRow[] = []; let oldNumber = 1; let newNumber = 1; let previousEnd = 1
  for (const line of diff) {
    const hunk = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line)
    if (hunk) { oldNumber = Number(hunk[1]); newNumber = Number(hunk[2]); const skipped = Math.max(0, oldNumber - previousEnd); if (skipped > 0) rows.push({ kind: 'hunk', text: `${skipped} unmodified lines` }); continue }
    if (/^(?:diff |index |---|\+\+\+|\\)/.test(line)) continue
    if (line.startsWith('+')) rows.push({ kind: 'added', text: line.slice(1), newNumber: newNumber++ })
    else if (line.startsWith('-')) rows.push({ kind: 'removed', text: line.slice(1), oldNumber: oldNumber++ })
    else if (line.startsWith(' ')) rows.push({ kind: 'context', text: line.slice(1), oldNumber: oldNumber++, newNumber: newNumber++ })
    previousEnd = oldNumber
  }
  const collapsed: DiffRow[] = []; let context: DiffRow[] = []
  const flush = () => { if (context.length >= 4) collapsed.push({ kind: 'hunk', text: `${context.length} unmodified lines`, hidden: context }); else collapsed.push(...context); context = [] }
  for (const row of hideWhitespace ? withoutWhitespaceChanges(rows) : rows) { if (row.kind === 'context') context.push(row); else { flush(); collapsed.push(row) } }
  flush()
  return collapsed
}

function pairRows(rows: readonly DiffRow[]) {
  const pairs: { old?: DiffRow; next?: DiffRow; hunk?: DiffRow }[] = []
  let index = 0
  while (index < rows.length) {
    const row = rows[index]!
    if (row.kind === 'hunk') { pairs.push({ hunk: row }); index += 1; continue }
    if (row.kind === 'context') { pairs.push({ old: row, next: row }); index += 1; continue }
    const removedRows: DiffRow[] = []; const addedRows: DiffRow[] = []
    while (index < rows.length && rows[index]!.kind === 'removed') removedRows.push(rows[index++]!)
    while (index < rows.length && rows[index]!.kind === 'added') addedRows.push(rows[index++]!)
    for (let pair = 0; pair < Math.max(removedRows.length, addedRows.length); pair += 1) pairs.push({ old: removedRows[pair], next: addedRows[pair] })
  }
  return pairs
}

function CodeLine({ row, side, wrap, language }: { row: DiffRow; side?: 'old' | 'new'; wrap: boolean; language: string }) {
  const number = side === 'old' ? row.oldNumber : row.newNumber ?? row.oldNumber
  const sign = row.kind === 'added' ? '+' : row.kind === 'removed' ? '−' : ''
  return <div data-diff-kind={row.kind} {...stylex.props(styles.codeRow, row.kind === 'added' && styles.addedRow, row.kind === 'removed' && styles.removedRow)}><span data-diff-gutter {...stylex.props(styles.gutter, row.kind === 'added' && styles.addedGutter, row.kind === 'removed' && styles.removedGutter)}><span>{number ?? ''}</span><span>{sign}</span></span><code {...stylex.props(styles.codeText, wrap && styles.wrappedText)}><HighlightedSource code={row.text} language={language} /></code></div>
}

function ContextRow({ row, split = false, wrap, language }: { row: DiffRow; split?: boolean; wrap: boolean; language: string }) {
  const [open, setOpen] = React.useState(false)
  return row.hidden ? <><AriaButton aria-expanded={open} onPress={() => setOpen(value => !value)} {...stylex.props(styles.hunk)}>{open ? 'Hide unmodified lines' : row.text}</AriaButton>{open ? row.hidden.map((line, index) => split ? <div key={index} {...stylex.props(styles.split)}><div {...stylex.props(styles.splitCell)}><CodeLine row={line} side="old" wrap={wrap} language={language} /></div><div {...stylex.props(styles.splitCell)}><CodeLine row={line} side="new" wrap={wrap} language={language} /></div></div> : <CodeLine key={index} row={line} wrap={wrap} language={language} />) : null}</> : <div {...stylex.props(styles.hunk)}>{row.text}</div>
}

function FileDiff({ file, landmarkContext, collapsed, onToggle, layout, wrap, hideWhitespace, revealRequest }: { file: DiffFile; landmarkContext?: string; collapsed: boolean; onToggle: () => void; layout: 'unified' | 'split'; wrap: boolean; hideWhitespace: boolean; revealRequest?: DiffRevealRequest }) {
  const rows = React.useMemo(() => rowsFromDiff(file.diff, hideWhitespace), [file.diff, hideWhitespace])
  const language = React.useMemo(() => languageFromPath(file.path), [file.path])
  const splitRows = React.useMemo(() => layout === 'split' ? pairRows(rows) : [], [rows, layout])
  const codeId = React.useId()
  const lastScrolled = React.useRef<number | undefined>(undefined)
  const revealPath = revealRequest?.path
  const revealSequence = revealRequest?.sequence
  const revealFile = React.useCallback((node: HTMLElement | null) => {
    if (node && !collapsed && revealPath === file.path && revealSequence !== undefined && lastScrolled.current !== revealSequence) {
      node.scrollIntoView({ block: 'nearest', inline: 'nearest' })
      lastScrolled.current = revealSequence
    }
  }, [collapsed, file.path, revealPath, revealSequence])
  return <section ref={revealFile} aria-label={landmarkContext ? `${file.path}, ${landmarkContext}` : file.path} {...stylex.props(styles.fileSection)}>
    <div {...stylex.props(styles.fileHead)}><AriaButton aria-expanded={!collapsed} aria-controls={codeId} onPress={onToggle} {...stylex.props(styles.fileToggle)}><Icon name={collapsed ? 'chevron-right' : 'chevron-down'} /><span title={file.path} {...stylex.props(styles.path)}>{file.path}</span></AriaButton>{file.added !== 0 && <span {...stylex.props(styles.added)}>+{file.added}</span>}{file.removed !== 0 && <span {...stylex.props(styles.removed)}>−{file.removed}</span>}<IconButton icon="copy" label={`Copy file path ${file.path}`} onPress={() => { void navigator.clipboard.writeText(file.path) }} /></div>
    {!collapsed ? <div id={codeId} data-testid="diff-code" role="region" aria-label={`Diff lines for ${file.path}${landmarkContext ? `, ${landmarkContext}` : ''}`} tabIndex={0} {...stylex.props(styles.code)}>{rows.length === 0 ? <p {...stylex.props(styles.empty)}>{hideWhitespace && file.diff.length > 0 ? 'No non-whitespace changes.' : 'No diff lines supplied.'}</p> : layout === 'unified' ? rows.map((row, index) => row.kind === 'hunk' ? <ContextRow key={index} row={row} wrap={wrap} language={language} /> : <CodeLine key={index} row={row} wrap={wrap} language={language} />) : splitRows.map((pair, index) => pair.hunk ? <ContextRow key={index} row={pair.hunk} split wrap={wrap} language={language} /> : <div key={index} {...stylex.props(styles.split)}><div {...stylex.props(styles.splitCell)}>{pair.old ? <CodeLine row={pair.old} side="old" wrap={wrap} language={language} /> : null}</div><div {...stylex.props(styles.splitCell)}>{pair.next ? <CodeLine row={pair.next} side="new" wrap={wrap} language={language} /> : null}</div></div>)}</div> : null}
  </section>
}

export const DiffPanel = React.memo(function DiffPanel({ open, width = '100%', title = 'Changes', landmarkContext, diff, path, added, removed, currentTurnFiles, branchFiles, revealRequest, onOpenSurface, onClose, turns }: DiffPanelProps) {
  const [scope, setScope] = React.useState<DiffScope>('branch')
  const [layout, setLayout] = React.useState<'unified' | 'split'>('unified')
  const [wrap, setWrap] = React.useState(false)
  const [hideWhitespace, setHideWhitespace] = React.useState(false)
  const [surfaceKey, setSurfaceKey] = React.useState('diff')
  const [collapsed, setCollapsed] = React.useState<ReadonlySet<string>>(() => new Set())
  const [handledReveal, setHandledReveal] = React.useState<DiffRevealRequest>()
  const currentFiles = React.useMemo(() => {
    if (currentTurnFiles !== undefined) return currentTurnFiles
    if (turns?.length) return [turns[turns.length - 1]!]
    if (diff !== undefined && path !== undefined) return [{ diff, path, added: added ?? 0, removed: removed ?? 0 }]
    return undefined
  }, [currentTurnFiles, turns, diff, path, added, removed])
  const selectedScope = scope === 'branch' && branchFiles !== undefined ? 'branch' : currentFiles !== undefined ? 'current' : branchFiles !== undefined ? 'branch' : 'current'
  const files = (selectedScope === 'current' ? currentFiles : branchFiles) ?? []
  const scopes = [{ id: 'current', label: 'Current turn', available: currentFiles !== undefined }, { id: 'branch', label: 'Branch/working tree', available: branchFiles !== undefined }]
  const totalAdded = files.reduce((total, file) => total + file.added, 0)
  const totalRemoved = files.reduce((total, file) => total + file.removed, 0)
  if (revealRequest && (handledReveal?.path !== revealRequest.path || handledReveal?.sequence !== revealRequest.sequence)) {
    const revealScope = files.some(file => file.path === revealRequest.path) ? selectedScope : currentFiles?.some(file => file.path === revealRequest.path) ? 'current' : branchFiles?.some(file => file.path === revealRequest.path) ? 'branch' : undefined
    if (revealScope) {
      setHandledReveal(revealRequest)
      setScope(revealScope)
      setSurfaceKey('diff')
      setCollapsed(previous => { const next = new Set(previous); next.delete(`${revealScope}:${revealRequest.path}`); return next })
    }
  }
  if (!open) return null
  return <ThemePortal><aside aria-label={landmarkContext ? `${title}, ${landmarkContext}` : title} data-testid="diff-panel" {...stylex.props(styles.panel, styles.width(width))}>
    <Tabs selectedKey={surfaceKey} onSelectionChange={key => { setSurfaceKey(String(key)); onOpenSurface?.(String(key)) }} {...stylex.props(styles.tabRoot)}>
      <div {...stylex.props(styles.surfaceHead)}><h2 {...stylex.props(styles.title)}>{title}</h2><TabList aria-label="Surfaces" {...stylex.props(styles.tabs)}>{surfaceOptions.map(option => <Tab key={option.key} id={option.key} {...stylex.props(styles.tab)}>{option.label}</Tab>)}</TabList>{onClose && <IconButton icon="x" label={`Close ${title}`} onPress={onClose} />}</div>
      <TabPanel id="diff" {...stylex.props(styles.body)}>
        <div role="toolbar" aria-label="Diff options" {...stylex.props(styles.toolbar)}>
          <Select aria-label="Change scope" selectedKey={selectedScope} disabledKeys={scopes.filter(choice => !choice.available).map(choice => choice.id)} onSelectionChange={key => { if (key === 'current' || key === 'branch') setScope(key) }}><AriaButton {...stylex.props(styles.scopePicker)}><SelectValue /><Icon name="chevron-down" /></AriaButton><Popover {...stylex.props(styles.popover)}><ListBox items={scopes}>{choice => <ListBoxItem id={choice.id} textValue={choice.label} {...stylex.props(styles.option)}>{choice.label}{!choice.available ? <span {...stylex.props(styles.unavailable)}> — Not supplied</span> : null}</ListBoxItem>}</ListBox></Popover></Select>
          <span {...stylex.props(styles.fileCount)}>{files.length} {files.length === 1 ? 'file' : 'files'}</span>{totalAdded !== 0 && <span {...stylex.props(styles.added)}>+{totalAdded}</span>}{totalRemoved !== 0 && <span {...stylex.props(styles.removed)}>−{totalRemoved}</span>}<span {...stylex.props(styles.spacer)} />
          <Button aria-pressed={layout === 'unified'} onPress={() => setLayout('unified')}>Unified</Button><Button aria-pressed={layout === 'split'} onPress={() => setLayout('split')}>Split</Button>
          <Tooltip label="Wrap lines"><ToggleButton aria-label="Wrap lines" isSelected={wrap} onChange={setWrap} {...stylex.props(styles.iconToggle)}><Icon name="wrap" /></ToggleButton></Tooltip>
          <Tooltip label="Hide whitespace-only changes"><ToggleButton aria-label="Hide whitespace-only changes" isSelected={hideWhitespace} onChange={setHideWhitespace} {...stylex.props(styles.iconToggle)}><Icon name="whitespace" /></ToggleButton></Tooltip>
        </div>
        <div {...stylex.props(styles.files)}>{files.length === 0 ? <p {...stylex.props(styles.empty)}>{(selectedScope === 'current' ? currentFiles : branchFiles) !== undefined ? 'No changed files in this scope.' : 'No changes supplied for this scope.'}</p> : files.map(file => {
          const key = `${selectedScope}:${file.path}`
          return <FileDiff key={key} file={file} landmarkContext={landmarkContext} collapsed={collapsed.has(key)} onToggle={() => setCollapsed(previous => { const next = new Set(previous); if (next.has(key)) next.delete(key); else next.add(key); return next })} layout={layout} wrap={wrap} hideWhitespace={hideWhitespace} revealRequest={revealRequest?.path === file.path ? revealRequest : undefined} />
        })}</div>
      </TabPanel>
      {surfaceOptions.filter(option => option.key !== 'diff').map(option => <TabPanel key={option.key} id={option.key} {...stylex.props(styles.launcher)}><p>Open a surface</p>{surfaceOptions.map(surfaceOption => <Button key={surfaceOption.key} onPress={() => { setSurfaceKey(surfaceOption.key); onOpenSurface?.(surfaceOption.key) }}>{surfaceOption.label}<kbd>{surfaceOption.hint}</kbd></Button>)}</TabPanel>)}
    </Tabs>
  </aside></ThemePortal>
})

const styles = stylex.create({
  panel: { flexShrink: 0, height: '100%', backgroundColor: surface.canvas, display: 'flex', flexDirection: 'column', minHeight: 0, overflow: 'hidden' },
  width: (width: number | string) => ({ width }),
  tabRoot: { display: 'flex', flexDirection: 'column', minHeight: 0, height: '100%' },
  surfaceHead: { minHeight: g.controlLg, boxSizing: 'border-box', display: 'flex', alignItems: 'center', gap: s.md, paddingInline: s.lg, flexShrink: 0, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border },
  title: { margin: 0, fontSize: t.uiSize, fontWeight: t.weightMedium, color: text.fg, flexShrink: 0 },
  tabs: { display: 'flex', alignItems: 'center', gap: s.xs, minWidth: 0, overflowX: 'auto' },
  tab: { height: g.controlSm, maxWidth: g.tabMax, display: 'inline-flex', alignItems: 'center', paddingInline: s.md, color: text.fgMuted, borderRadius: r.control, fontSize: t.metaSize, whiteSpace: 'nowrap', flexShrink: 0, cursor: 'pointer', ':hover': { color: text.fg }, ':is([data-selected])': { backgroundColor: surface.rowActive, color: text.fg }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  body: { display: 'flex', flexDirection: 'column', minHeight: 0, flexGrow: 1 },
  toolbar: { minHeight: g.controlLg, boxSizing: 'border-box', display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: s.xs, paddingInline: s.lg, paddingBlock: 0, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border, flexShrink: 0 },
  iconToggle: { width: g.controlSm, height: g.controlSm, display: 'inline-flex', alignItems: 'center', justifyContent: 'center', flexShrink: 0, padding: 0, borderWidth: 0, borderRadius: r.control, backgroundColor: 'transparent', color: text.fgMuted, cursor: 'pointer', ':is([data-selected])': { backgroundColor: surface.rowActive, color: text.fg }, ':hover': { backgroundColor: surface.rowHover, color: text.fg }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  scopePicker: { display: 'flex', alignItems: 'center', gap: s.xs, backgroundColor: 'transparent', borderWidth: 0, color: text.fgMuted, height: g.controlMd, fontSize: t.metaSize, cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  popover: { backgroundColor: surface.raised, borderRadius: r.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, padding: s.xs, color: text.fg },
  option: { padding: s.md, fontSize: t.metaSize, borderRadius: r.sm, cursor: 'pointer', ':is([data-focused])': { backgroundColor: surface.rowHover }, ':is([data-disabled])': { color: text.fgFaint, cursor: 'default' } },
  unavailable: { color: text.fgFaint },
  fileCount: { fontSize: t.metaSize, color: text.fgMuted, fontVariantNumeric: 'tabular-nums' },
  files: { minHeight: 0, flexGrow: 1, overflowY: 'auto' },
  fileSection: { borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border },
  fileHead: { minHeight: g.controlLg, display: 'flex', alignItems: 'center', gap: s.md, paddingInline: s.lg },
  fileToggle: { display: 'flex', alignItems: 'center', gap: s.sm, flexGrow: 1, minWidth: 0, padding: 0, minHeight: g.controlMd, backgroundColor: 'transparent', borderWidth: 0, color: text.fgMuted, textAlign: 'left', cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  path: { fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, color: text.fg, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  added: { fontFamily: t.fontMono, fontSize: t.denseSize, color: status.diffAdded, fontVariantNumeric: 'tabular-nums', flexShrink: 0 },
  removed: { fontFamily: t.fontMono, fontSize: t.denseSize, color: text.fgMuted, fontVariantNumeric: 'tabular-nums', flexShrink: 0 },
  spacer: { flexGrow: 1 },
  code: { minHeight: 0, overflowX: 'auto', fontFamily: t.fontMono, fontSize: t.codeSize, lineHeight: t.uiLeading, paddingBottom: s.lg, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: `calc(0px - ${g.focusRing})` } },
  codeRow: { display: 'flex', alignItems: 'stretch', minHeight: t.uiLeading, minWidth: '100%' },
  addedRow: { backgroundColor: `color-mix(in srgb, ${status.diffAddTint} 9%, transparent)` },
  removedRow: { backgroundColor: status.diffRemovedWash },
  gutter: { width: g.gutter40, minWidth: g.gutter40, flexShrink: 0, boxSizing: 'border-box', display: 'flex', alignItems: 'flex-start', justifyContent: 'space-between', gap: s.xs2, paddingInline: s.xs, color: text.fgFaint, fontVariantNumeric: 'tabular-nums', userSelect: 'none', borderLeftWidth: g.focusRing, borderLeftStyle: 'solid', borderLeftColor: 'transparent' },
  addedGutter: { backgroundColor: surface.washSubtle, borderLeftColor: status.diffAddTint, color: status.diffAddTint },
  removedGutter: { backgroundColor: surface.washSubtle, borderLeftColor: status.diffRemoved, color: status.diffRemoved },
  codeText: { fontFamily: 'inherit', fontSize: 'inherit', lineHeight: 'inherit', paddingInline: s.sm, color: text.fgSoft, whiteSpace: 'pre', flexGrow: 1 },
  wrappedText: { whiteSpace: 'pre-wrap', overflowWrap: 'anywhere', minWidth: 0, flexBasis: 0 },
  hunk: { width: '100%', minHeight: g.toolRow, display: 'flex', alignItems: 'center', justifyContent: 'center', fontFamily: t.fontSans, fontSize: t.denseSize, color: text.fgFaint, borderWidth: 0, backgroundColor: 'transparent', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  split: { display: 'grid', gridTemplateColumns: 'minmax(0, 1fr) minmax(0, 1fr)' },
  splitCell: { minWidth: 0, overflowX: 'auto', borderRightWidth: g.hairline, borderRightStyle: 'solid', borderRightColor: border.border },
  empty: { margin: 0, padding: s.lg, fontFamily: t.fontSans, fontSize: t.metaSize, color: text.fgMuted },
  launcher: { display: 'flex', flexDirection: 'column', alignItems: 'flex-start', gap: s.sm, padding: s.xl, color: text.fgMuted, fontSize: t.uiSize },
})
