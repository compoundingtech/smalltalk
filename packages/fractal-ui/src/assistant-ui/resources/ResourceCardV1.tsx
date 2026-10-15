// Controlled presentation; no fixture imports.
/** Locked: resources stay visible outside folded work, paths are mono, hover is supplementary, click opens the split. Dimensions: RC structure, resource kind, state. Default RC-1. */
import * as stylex from '@stylexjs/stylex'
import * as React from 'react'
import { Button, Dialog, Heading, Modal, ModalOverlay, Tooltip, TooltipTrigger, VisuallyHidden } from 'react-aria-components'
import { surfaceVars as surface, textVars as ink, borderVars as border, accentVars as accent, statusVars as status, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g } from '../composition-tokens.stylex'
import type { ResourceData as TasteResource, ResourceFile as TasteFile, ResourceState as TasteState, ResourceMetadata } from './resource-model.ts'
import { Icon, type IconName } from '../composition/Icons'
export type ResourceVariant = 'RC-1' | 'RC-2' | 'RC-3'
export const resourceDescriptions: Record<ResourceVariant, string> = { 'RC-1': '40px flat wash card; open a resource in the split', 'RC-2': 'Hairline outline with up to three visible file rows', 'RC-3': '20px baseline-aligned file chips with six-line diff hover previews' }
function FileIcon() {
  return <svg aria-hidden="true" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinejoin="round" {...stylex.props(styles.icon)}><path d="M9.5 2H4a1 1 0 0 0-1 1v10a1 1 0 0 0 1 1h8a1 1 0 0 0 1-1V5.5z" /><path d="M9.5 2v3.5H13" /></svg>
}
export function ResourceHoverV1({ resource, file = resource.files[0] }: { resource: TasteResource; file?: TasteFile }) {
  return <><strong {...stylex.props(styles.hoverTitle)}>{resource.label}</strong><div data-resource-detail {...stylex.props(styles.detail, styles.meta)}>{resource.detail}</div>{file !== undefined && <><div {...stylex.props(styles.path)}>{file.path}</div>{file.lines._tag === 'Known' && <pre {...stylex.props(styles.preview)}>{file.lines.value.slice(0, 6).map((line, index) => <div key={index} {...stylex.props(line.startsWith('+') && styles.addedRow, line.startsWith('-') && styles.removedRow)}>{line}</div>)}</pre>}</>}</>
}
/** Without a host split callback, standalone cards open the same real file content in an accessible dialog. */
function ResourceAction({ resource, file, onOpen, children, chip = false, row = false, titlePriority = false, label, detailLineId }: { resource: TasteResource; file?: TasteFile; onOpen?: (resource: TasteResource, file?: TasteFile) => void; children: React.ReactNode; chip?: boolean; row?: boolean; titlePriority?: boolean; label: string; detailLineId?: string }) {
  const [isOpen, setOpen] = React.useState(false)
  const descriptionId=React.useId()
  // The custom hover is the only hover disclosure; the dialog keeps the full detail as its description.
  const detailId = React.useId()
  return <>{chip&&file!==undefined&&<span id={descriptionId} hidden>{file.path}</span>}<TooltipTrigger delay={150}><Button data-details-resource-row={titlePriority ? true : undefined} aria-describedby={detailLineId ?? (chip&&file!==undefined ? descriptionId : undefined)} onPress={() => onOpen === undefined ? setOpen(true) : onOpen(resource, file)} {...stylex.props(chip ? styles.chip : row ? styles.file : styles.summary, titlePriority && styles.priorityRow, detailLineId !== undefined && styles.detailRow)}>{children}</Button><Tooltip {...stylex.props(styles.hover)}><ResourceHoverV1 resource={resource} file={file} /></Tooltip></TooltipTrigger><ModalOverlay isOpen={isOpen} onOpenChange={setOpen} isDismissable {...stylex.props(styles.overlay)}><Modal {...stylex.props(styles.modal)}><Dialog aria-label={`Resource ${label}`} aria-describedby={detailId} {...stylex.props(styles.dialog)}>{({ close }) => <><Heading slot="title" {...stylex.props(styles.hoverTitle)}>{resource.label}</Heading><VisuallyHidden><span id={detailId}>{resource.detail}</span></VisuallyHidden><div data-resource-detail {...stylex.props(styles.detail, styles.meta)}>{resource.detail}</div>{(file === undefined ? resource.files : [file]).map(item => <section key={item.path}><div {...stylex.props(styles.path)}>{item.path}</div>{item.lines._tag === 'Known' && <pre {...stylex.props(styles.preview)}>{item.lines.value.map((line, index) => <div key={index} {...stylex.props(line.startsWith('+') && styles.addedRow, line.startsWith('-') && styles.removedRow)}>{line}</div>)}</pre>}</section>)}<Button onPress={close} {...stylex.props(styles.close)}>Close</Button></>}</Dialog></Modal></ModalOverlay></>
}
function sumCounts(files: readonly TasteFile[], field: 'added' | 'removed'): ResourceMetadata<number> {
  let value = 0
  if (files.length === 0) return { _tag: 'Unknown' }
  for (const file of files) {
    const count = file[field]
    if (count._tag === 'Unknown') return count
    value += count.value
  }
  return { _tag: 'Known', value }
}

/** Supplementary metadata is readable in full or absent; intrinsic fit is local presentation state. */
export function WholeResourceMeta({ children, numeric = false, styles: callerStyles }: { children: React.ReactNode; numeric?: boolean; styles?: stylex.StyleXStyles }) {
  const observeFit = React.useCallback((slot: HTMLSpanElement | null) => {
    if (slot === null) return
    const row = slot.parentElement!
    const title = row.querySelector<HTMLElement>('[data-details-resource-title]')!
    const value = slot.firstElementChild as HTMLSpanElement
    const intrinsicTitle = document.createElement('span')
    intrinsicTitle.className = stylex.props(styles.intrinsicTitle).className ?? ''
    intrinsicTitle.setAttribute('aria-hidden', 'true')
    row.append(intrinsicTitle)
    const titleClassName = title.className
    const update = () => {
      intrinsicTitle.textContent = title.textContent
      const rowStyle = getComputedStyle(row)
      const width = row.clientWidth - parseFloat(rowStyle.paddingLeft) - parseFloat(rowStyle.paddingRight)
      const gap = parseFloat(rowStyle.columnGap) || 0
      const reservedTitle = Math.min(intrinsicTitle.getBoundingClientRect().width, width * 0.6 + gap)
      const reservation = stylex.props(styles.titleReservation(reservedTitle))
      title.className = `${titleClassName} ${reservation.className ?? ''}`
      for (const [property, value] of Object.entries(reservation.style ?? {})) title.style.setProperty(property, String(value))
      const siblings = Array.from(row.children).filter(child => child !== title && child !== slot && child !== intrinsicTitle)
      const occupied = siblings.reduce((sum, child) => sum + child.getBoundingClientRect().width, 0)
      const available = width - reservedTitle - occupied - gap * (siblings.length + 1)
      const metadataWidth = value.getBoundingClientRect().width
      const fits = metadataWidth > 0 && metadataWidth <= available + 0.01
      slot.className = stylex.props(styles.wholeMetaSlot, numeric && styles.numericMeta, callerStyles, !fits && styles.absentMeta).className ?? ''
      value.className = stylex.props(styles.wholeMetaValue).className ?? ''
      value.setAttribute('aria-hidden', String(!fits))
      slot.setAttribute('data-resource-meta-fit', fits ? 'whole' : 'absent')
    }
    const observer = new ResizeObserver(update)
    observer.observe(row)
    observer.observe(value)
    observer.observe(intrinsicTitle)
    update()
    return () => {
      observer.disconnect()
      intrinsicTitle.remove()
      title.className = titleClassName
      for (const property of Object.keys(stylex.props(styles.titleReservation(0)).style ?? {})) title.style.removeProperty(property)
    }
  }, [numeric, callerStyles, children])
  return <span ref={observeFit} data-details-resource-meta data-resource-meta={numeric ? 'numeric' : 'descriptive'} {...stylex.props(styles.wholeMetaSlot, numeric && styles.numericMeta, callerStyles)}><span data-resource-meta-value aria-hidden="true" {...stylex.props(styles.wholeMetaValue, styles.hiddenMeta)}>{children}</span></span>
}
/** A detail line paints `resource.detail` under the title when it carries reported facts that hover must not hide; it describes the open action. */
export function ResourceCardV1({ resource, variant = 'RC-1', titlePriority = false, titleLines = 1, icon, detailLine = false, onOpen }: { resource: TasteResource; variant?: ResourceVariant; state?: TasteState; titlePriority?: boolean; titleLines?: 1 | 'wrap'; icon?: IconName; onOpen?: (resource: TasteResource, file?: TasteFile) => void } & ({ detailLine?: false } | { detailLine: boolean; titlePriority: true; icon: IconName })) {
  const detailId = React.useId()
  const added = sumCounts(resource.files, 'added')
  const removed = sumCounts(resource.files, 'removed')
  const label = resource.kind === 'diff' ? resource.fileCount._tag === 'Known' ? `${resource.fileCount.value} changed ${resource.fileCount.value === 1 ? 'file' : 'files'}` : 'Changed files' : resource.label
  const counts = <>{added._tag === 'Known' && (!titlePriority || added.value !== 0) && <span {...stylex.props(styles.count, styles.added, titlePriority && styles.priorityCount)}>+{added.value}</span>}{removed._tag === 'Known' && (!titlePriority || removed.value !== 0) && <span {...stylex.props(styles.count, styles.removed, titlePriority && styles.priorityCount)}>−{removed.value}</span>}</>
  if (variant === 'RC-3') return <div {...stylex.props(styles.chips)}>{resource.kind !== 'diff' && <span {...stylex.props(styles.detail)}>{resource.label}</span>}{resource.files.map(file => <ResourceChipV1 key={file.path} resource={resource} file={file} onOpen={onOpen} />)}</div>
  return <div data-resource-card {...stylex.props(styles.card, variant === 'RC-1' ? styles.wash : styles.outline)}>
    <ResourceAction resource={resource} onOpen={onOpen} label={label} titlePriority={titlePriority} detailLineId={detailLine ? detailId : undefined}>{icon === undefined ? <FileIcon /> : <span data-details-resource-icon={icon} {...stylex.props(styles.resourceIcon)}><Icon name={icon} /></span>}<span data-details-resource-title={titlePriority ? true : undefined} data-resource-title-lines={titleLines} {...stylex.props(styles.title, titlePriority ? styles.priorityTitle : styles.standardTitle, titleLines === 'wrap' && styles.wrappedTitle)}>{label}</span>{titlePriority ? <WholeResourceMeta numeric>{counts}</WholeResourceMeta> : counts}<span {...stylex.props(styles.detail, styles.action, titlePriority && styles.priorityAction)}>Open</span></ResourceAction>
    {detailLine && <p id={detailId} data-resource-detail {...stylex.props(styles.detail, styles.detailLine)}>{resource.detail}</p>}
    {variant === 'RC-2' && resource.files.slice(0, 3).map(file => <ResourceAction key={file.path} resource={resource} file={file} onOpen={onOpen} row label={file.path}><FileIcon /><span {...stylex.props(styles.title, styles.path)}>{file.path}</span>{file.added._tag === 'Known' && <span {...stylex.props(styles.count, styles.added)}>+{file.added.value}</span>}{file.removed._tag === 'Known' && <span {...stylex.props(styles.count, styles.removed)}>−{file.removed.value}</span>}<span {...stylex.props(styles.detail, styles.action)}>Open</span></ResourceAction>)}
  </div>
}
/** The identical chip is valid both after the answer and inside prose. */
export function ResourceChipV1({ resource, file, onOpen }: { resource: TasteResource; file: TasteFile; onOpen?: (resource: TasteResource, file?: TasteFile) => void }) {
  return <ResourceAction resource={resource} file={file} onOpen={onOpen} chip label={file.path}><FileIcon /><span>{file.path.split('/').at(-1)}</span>{file.added._tag === 'Known' && <span {...stylex.props(styles.added)}>+{file.added.value}</span>}{file.removed._tag === 'Known' && <span {...stylex.props(styles.removed)}>−{file.removed.value}</span>}</ResourceAction>
}
const styles = stylex.create({
  card: { position: 'relative', borderRadius: r.md, overflow: 'hidden', minWidth: 0 }, wash: { backgroundColor: surface.washSubtle }, outline: { borderWidth: 1, borderStyle: 'solid', borderColor: border.border },
  summary: { minHeight: g.footer, display: 'flex', alignItems: 'center', gap: s.md, width: '100%', paddingInline: s.lg, backgroundColor: surface.transparent, color: ink.fgSoft, borderWidth: 0, textAlign: 'left', cursor: 'pointer', fontFamily: t.fontSans, fontSize: t.uiSize, ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outline: `2px solid ${accent.primary}`, outlineOffset: -2 } },
  title: { flexGrow: 1, minWidth: 0, overflow: 'hidden', whiteSpace: 'nowrap', textOverflow: 'ellipsis' }, count: { fontFamily: t.fontMono, fontSize: t.metaSize, flexShrink: 0, minWidth: '4ch', textAlign: 'right' }, action: { width: g.footer, flexShrink: 0, textAlign: 'right', whiteSpace: 'nowrap' }, added: { color: ink.fgMuted }, removed: { color: ink.fgMuted }, detail: { fontSize: t.metaSize, color: ink.fgMuted, lineHeight: t.metaLeading },
  priorityRow: { boxSizing: 'border-box', minWidth: 0, gap: s.xs, paddingInline: s.sm },
  priorityTitle: { flexBasis: 0, flexShrink: 0 },
  priorityAction: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', boxSizing: 'border-box', width: 'auto', minHeight: g.controlSm, paddingInline: s.sm, borderWidth: 1, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.control, backgroundColor: surface.controlFill, color: ink.fgSoft, whiteSpace: 'nowrap' },
  // The action row tightens to one control height so the title and its painted facts read as one two-line card.
  detailRow: { minHeight: g.controlLg },
  // Facts align under the title (row inset + kit icon + row gap) and wrap between words; only an overlong token breaks.
  detailLine: { margin: s.zero, paddingBlockEnd: s.sm, paddingInlineStart: `calc(${s.sm} + ${g.icon} + ${s.xs})`, paddingInlineEnd: s.sm, overflowWrap: 'break-word', textWrap: 'pretty' },
  addedRow: { backgroundColor: `color-mix(in srgb, ${status.diffAddTint} 9%, transparent)`, borderLeftWidth: g.focusRing, borderLeftStyle: 'solid', borderLeftColor: status.diffAddTint },
  removedRow: { backgroundColor: status.diffRemovedWash, borderLeftWidth: g.focusRing, borderLeftStyle: 'solid', borderLeftColor: status.diffRemoved },
  standardTitle: { minWidth: 0 },
  wrappedTitle: { whiteSpace: 'normal', textOverflow: 'clip', overflowWrap: 'anywhere', textWrap: 'balance', lineHeight: t.uiLeading },
  resourceIcon: { display: 'inline-flex', flexShrink: 0 },
  wholeMetaSlot: { minWidth: 0, flexShrink: 0, whiteSpace: 'nowrap', overflow: 'hidden', fontFamily: t.fontSans, fontSize: t.metaSize },
  wholeMetaValue: { display: 'inline-flex', width: 'max-content', whiteSpace: 'nowrap' },
  hiddenMeta: { visibility: 'hidden' },
  absentMeta: { position: 'absolute', width: 0, visibility: 'hidden', pointerEvents: 'none' },
  intrinsicTitle: { position: 'absolute', width: 'max-content', whiteSpace: 'nowrap', visibility: 'hidden', pointerEvents: 'none' },
  titleReservation: (width: number) => ({ minWidth: width }),
  numericMeta: { fontFamily: t.fontMono },
  priorityCount: { minWidth: 0, marginInlineStart: s.xs },
  file: { width: '100%', minHeight: g.toolRow, paddingInline: s.lg, display: 'flex', alignItems: 'center', gap: s.md, backgroundColor: surface.transparent, color: ink.fgMuted, borderWidth: 0, cursor: 'pointer', textAlign: 'left', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outline: `2px solid ${accent.primary}`, outlineOffset: -2 } },
  path: { fontFamily: t.fontMono, fontSize: t.metaSize, lineHeight: t.metaLeading }, chips: { display: 'flex', flexWrap: 'wrap', alignItems: 'baseline', gap: s.md }, chip: { display: 'inline-flex', alignItems: 'center', verticalAlign: 'baseline', boxSizing: 'border-box', height: s.xxl, lineHeight: t.metaLeading, gap: s.md, paddingBlock: s.zero, paddingInline: s.sm, borderRadius: r.sm, borderWidth: 0, backgroundColor: surface.controlFill, color: ink.fgSoft, fontFamily: t.fontMono, fontSize: t.metaSize, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outline: `2px solid ${accent.primary}` } },
  hover: { maxWidth: 'min(480px, calc(100vw - 24px))', padding: s.lg, backgroundColor: surface.raised, color: ink.fg, borderRadius: r.md, borderWidth: 1, borderStyle: 'solid', borderColor: border.borderStrong, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.uiLeading, zIndex: 10 }, preview: { margin: 0, marginTop: s.md, padding: s.md, backgroundColor: surface.codeBg, borderRadius: r.sm, fontFamily: t.fontMono, fontSize: t.metaSize, lineHeight: t.metaLeading, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' },
  icon: { flexShrink: 0 }, hoverTitle: { fontWeight: 600, fontSize: t.uiSize, margin: s.zero }, meta: { minWidth: 0, overflow: 'hidden', whiteSpace: 'nowrap', textOverflow: 'ellipsis' },
  overlay: { position: 'fixed', inset: 0, zIndex: 20, backgroundColor: surface.scrim, display: 'flex', alignItems: 'center', justifyContent: 'center', padding: s.xl },
  modal: { width: g.modalMax, maxWidth: '100%', maxHeight: '80dvh', overflowY: 'auto', backgroundColor: surface.raised, color: ink.fg, borderRadius: r.md, borderWidth: 1, borderStyle: 'solid', borderColor: border.borderStrong },
  dialog: { padding: s.xl, display: 'flex', flexDirection: 'column', gap: s.md, fontFamily: t.fontSans, outline: 'none' },
  close: { alignSelf: 'flex-end', padding: s.md, borderRadius: r.sm, borderWidth: 0, backgroundColor: surface.controlFill, color: ink.fg, cursor: 'pointer', ':focus-visible': { outline: `2px solid ${accent.primary}` } },
})
