// Controlled presentation; no fixture imports.
/** Locked: resources stay visible outside folded work, paths are mono, hover is supplementary, click opens the split. Dimensions: RC structure, resource kind, state. Default RC-1. */
import * as stylex from '@stylexjs/stylex'
import * as React from 'react'
import { Button, Dialog, Heading, Modal, ModalOverlay, Tooltip, TooltipTrigger } from 'react-aria-components'
import { surfaceVars as surface, textVars as ink, borderVars as border, accentVars as accent, statusVars as status, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g } from '../composition-tokens.stylex'
import type { ResourceData as TasteResource, ResourceFile as TasteFile, ResourceState as TasteState, ResourceMetadata } from './resource-model.ts'
export type ResourceVariant = 'RC-1' | 'RC-2' | 'RC-3'
export const resourceDescriptions: Record<ResourceVariant, string> = { 'RC-1': '40px flat wash card; open a resource in the split', 'RC-2': 'Hairline outline with up to three visible file rows', 'RC-3': '20px baseline-aligned file chips with six-line diff hover previews' }
function FileIcon() {
  return <svg aria-hidden="true" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinejoin="round" {...stylex.props(styles.icon)}><path d="M9.5 2H4a1 1 0 0 0-1 1v10a1 1 0 0 0 1 1h8a1 1 0 0 0 1-1V5.5z" /><path d="M9.5 2v3.5H13" /></svg>
}
export function ResourceHoverV1({ resource, file = resource.files[0] }: { resource: TasteResource; file?: TasteFile }) {
  return <><strong {...stylex.props(styles.hoverTitle)}>{resource.label}</strong><div title={resource.detail} {...stylex.props(styles.detail, styles.meta)}>{resource.detail}</div>{file !== undefined && <><div {...stylex.props(styles.path)}>{file.path}</div><pre {...stylex.props(styles.preview)}>{(file.lines._tag === 'Known' ? file.lines.value.slice(0, 6) : ['Content unknown']).map((line, index) => <div key={index} {...stylex.props(line.startsWith('+') && styles.added, line.startsWith('-') && styles.removed)}>{line}</div>)}</pre></>}</>
}
/** Without a host split callback, standalone cards open the same real file content in an accessible dialog. */
function ResourceAction({ resource, file, onOpen, children, chip = false, row = false, label }: { resource: TasteResource; file?: TasteFile; onOpen?: (resource: TasteResource, file?: TasteFile) => void; children: React.ReactNode; chip?: boolean; row?: boolean; label: string }) {
  const [isOpen, setOpen] = React.useState(false)
  return <><TooltipTrigger delay={150}><Button aria-label={`Open ${label}`} onPress={() => onOpen === undefined ? setOpen(true) : onOpen(resource, file)} {...stylex.props(chip ? styles.chip : row ? styles.file : styles.summary)}>{children}</Button><Tooltip {...stylex.props(styles.hover)}><ResourceHoverV1 resource={resource} file={file} /></Tooltip></TooltipTrigger><ModalOverlay isOpen={isOpen} onOpenChange={setOpen} isDismissable {...stylex.props(styles.overlay)}><Modal {...stylex.props(styles.modal)}><Dialog aria-label={`Resource ${label}`} {...stylex.props(styles.dialog)}>{({ close }) => <><Heading slot="title" {...stylex.props(styles.hoverTitle)}>{resource.label}</Heading><div {...stylex.props(styles.detail, styles.meta)} title={resource.detail}>{resource.detail}</div>{(file === undefined ? resource.files : [file]).map(item => <section key={item.path}><div {...stylex.props(styles.path)}>{item.path}</div><pre {...stylex.props(styles.preview)}>{(item.lines._tag === 'Known' ? item.lines.value : ['Content unknown']).map((line, index) => <div key={index} {...stylex.props(line.startsWith('+') && styles.added, line.startsWith('-') && styles.removed)}>{line}</div>)}</pre></section>)}<Button onPress={close} {...stylex.props(styles.close)}>Close</Button></>}</Dialog></Modal></ModalOverlay></>
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
export function ResourceCardV1({ resource, variant = 'RC-1', onOpen }: { resource: TasteResource; variant?: ResourceVariant; state?: TasteState; onOpen?: (resource: TasteResource, file?: TasteFile) => void }) {
  const added = sumCounts(resource.files, 'added')
  const removed = sumCounts(resource.files, 'removed')
  const label = resource.kind === 'diff' ? resource.fileCount._tag === 'Known' ? `${resource.fileCount.value} changed ${resource.fileCount.value === 1 ? 'file' : 'files'}` : 'Changed files · count unknown' : resource.label
  if (variant === 'RC-3') return <div {...stylex.props(styles.chips)}>{resource.kind !== 'diff' && <span {...stylex.props(styles.detail)}>{resource.label}</span>}{resource.files.map(file => <ResourceChipV1 key={file.path} resource={resource} file={file} onOpen={onOpen} />)}</div>
  return <div {...stylex.props(styles.card, variant === 'RC-1' ? styles.wash : styles.outline)}>
    <ResourceAction resource={resource} onOpen={onOpen} label={label}><FileIcon /><span {...stylex.props(styles.title)}>{label}</span><span {...stylex.props(styles.count, styles.added)}>{added._tag === 'Known' ? `+${added.value}` : 'Unknown'}</span><span {...stylex.props(styles.count, styles.removed)}>{removed._tag === 'Known' ? `−${removed.value}` : 'Unknown'}</span><span {...stylex.props(styles.detail, styles.action)}>Open</span></ResourceAction>
    {variant === 'RC-2' && resource.files.slice(0, 3).map(file => <ResourceAction key={file.path} resource={resource} file={file} onOpen={onOpen} row label={file.path}><FileIcon /><span {...stylex.props(styles.title, styles.path)}>{file.path}</span><span {...stylex.props(styles.count, styles.added)}>{file.added._tag === 'Known' ? `+${file.added.value}` : 'Unknown'}</span><span {...stylex.props(styles.count, styles.removed)}>{file.removed._tag === 'Known' ? `−${file.removed.value}` : 'Unknown'}</span><span {...stylex.props(styles.detail, styles.action)}>Open</span></ResourceAction>)}
  </div>
}
/** The identical chip is valid both after the answer and inside prose. */
export function ResourceChipV1({ resource, file, onOpen }: { resource: TasteResource; file: TasteFile; onOpen?: (resource: TasteResource, file?: TasteFile) => void }) {
  return <ResourceAction resource={resource} file={file} onOpen={onOpen} chip label={file.path}><FileIcon /><span>{file.path.split('/').at(-1)}</span><span {...stylex.props(styles.added)}>{file.added._tag === 'Known' ? `+${file.added.value}` : 'Unknown'}</span><span {...stylex.props(styles.removed)}>{file.removed._tag === 'Known' ? `−${file.removed.value}` : 'Unknown'}</span></ResourceAction>
}
const styles = stylex.create({
  card: { borderRadius: r.md, overflow: 'hidden', minWidth: 0 }, wash: { backgroundColor: surface.washSubtle }, outline: { borderWidth: 1, borderStyle: 'solid', borderColor: border.border },
  summary: { minHeight: g.footer, display: 'flex', alignItems: 'center', gap: s.md, width: '100%', paddingInline: s.lg, backgroundColor: surface.transparent, color: ink.fgSoft, borderWidth: 0, textAlign: 'left', cursor: 'pointer', fontFamily: t.fontSans, fontSize: t.uiSize, ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outline: `2px solid ${accent.primary}`, outlineOffset: -2 } },
  title: { flexGrow: 1, minWidth: 0, overflow: 'hidden', whiteSpace: 'nowrap', textOverflow: 'ellipsis' }, count: { fontFamily: t.fontMono, fontSize: t.metaSize, flexShrink: 0, minWidth: '4ch', textAlign: 'right' }, action: { width: g.footer, flexShrink: 0, textAlign: 'right', whiteSpace: 'nowrap' }, added: { color: status.diffAdded }, removed: { color: status.dangerFg }, detail: { fontSize: t.metaSize, color: ink.fgMuted, lineHeight: t.metaLeading },
  file: { width: '100%', minHeight: g.toolRow, paddingInline: s.lg, display: 'flex', alignItems: 'center', gap: s.md, backgroundColor: surface.transparent, color: ink.fgMuted, borderWidth: 0, cursor: 'pointer', textAlign: 'left', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outline: `2px solid ${accent.primary}`, outlineOffset: -2 } },
  path: { fontFamily: t.fontMono, fontSize: t.metaSize, lineHeight: t.metaLeading }, chips: { display: 'flex', flexWrap: 'wrap', alignItems: 'baseline', gap: s.md }, chip: { display: 'inline-flex', alignItems: 'center', verticalAlign: 'baseline', boxSizing: 'border-box', height: s.xxl, lineHeight: t.metaLeading, gap: s.md, paddingBlock: s.zero, paddingInline: s.sm, borderRadius: r.sm, borderWidth: 0, backgroundColor: surface.controlFill, color: ink.fgSoft, fontFamily: t.fontMono, fontSize: t.metaSize, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outline: `2px solid ${accent.primary}` } },
  hover: { maxWidth: 'min(480px, calc(100vw - 24px))', padding: s.lg, backgroundColor: surface.raised, color: ink.fg, borderRadius: r.md, borderWidth: 1, borderStyle: 'solid', borderColor: border.borderStrong, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.uiLeading, zIndex: 10 }, preview: { margin: 0, marginTop: s.md, padding: s.md, backgroundColor: surface.codeBg, borderRadius: r.sm, fontFamily: t.fontMono, fontSize: t.metaSize, lineHeight: t.metaLeading, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' },
  icon: { flexShrink: 0 }, hoverTitle: { fontWeight: 600, fontSize: t.uiSize, margin: s.zero }, meta: { minWidth: 0, overflow: 'hidden', whiteSpace: 'nowrap', textOverflow: 'ellipsis' },
  overlay: { position: 'fixed', inset: 0, zIndex: 20, backgroundColor: surface.scrim, display: 'flex', alignItems: 'center', justifyContent: 'center', padding: s.xl },
  modal: { width: g.modalMax, maxWidth: '100%', maxHeight: '80dvh', overflowY: 'auto', backgroundColor: surface.raised, color: ink.fg, borderRadius: r.md, borderWidth: 1, borderStyle: 'solid', borderColor: border.borderStrong },
  dialog: { padding: s.xl, display: 'flex', flexDirection: 'column', gap: s.md, fontFamily: t.fontSans, outline: 'none' },
  close: { alignSelf: 'flex-end', padding: s.md, borderRadius: r.sm, borderWidth: 0, backgroundColor: surface.controlFill, color: ink.fg, cursor: 'pointer', ':focus-visible': { outline: `2px solid ${accent.primary}` } },
})
