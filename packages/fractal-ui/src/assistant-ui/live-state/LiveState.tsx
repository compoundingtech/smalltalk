import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button, Dialog, DialogTrigger, Heading, Popover } from 'react-aria-components'
import { SidebarStatus } from '../sidebar/SidebarStatus'
import { accentVars as accent, borderVars as border, geometryVars as g, radiusVars as r, spaceVars as s, statusVars as tone, surfaceVars as surface, textVars as ink, typeVars as t } from '../composition-tokens.stylex'
import { liveStateModel, type DiagnosticLine, type LiveStateModel } from './model'
import type { LiveStateSnapshot } from './types'

export interface LiveStateProps {
  readonly snapshot: LiveStateSnapshot
  /** Host clock, shared with existing sidebar/header status clocks. No component-owned polling. */
  readonly now: number
  readonly variant?: 'row' | 'header'
  readonly label: string
  readonly style?: stylex.StyleXStyles
}
function StateLine({ line, detailed = false }: { readonly line: DiagnosticLine; readonly detailed?: boolean }) {
  return <span data-diagnostic-axis={line.axis} data-stale={line.stale} {...stylex.props(styles.line, detailed && styles.detailLine, line.stale && styles.stale, (line.priority === 0 || line.priority === 1) && styles.danger, line.priority === 2 && styles.attention)}>
    <span {...stylex.props(styles.text, detailed && styles.detailText)}>{line.text}</span>
    {line.timer !== undefined && <span role="timer" aria-label={line.timer.label} data-timer-kind={line.timer.kind} {...stylex.props(styles.timer)}>{line.timer.text}</span>}
  </span>
}
/** Same definition-list semantics as sidebar Details; missing observations are explicit,
 * axis-specific sentences, not placeholder values or inferred idle/healthy states.
 */
export function LiveStateDetails({ model }: { readonly model: LiveStateModel }) {
  return <div data-testid="live-state-details"><dl {...stylex.props(styles.details)}>{model.lines.map(line => <React.Fragment key={line.axis}>
    <dt {...stylex.props(styles.label)}>{line.label}</dt><dd data-detail-axis={line.axis} {...stylex.props(styles.value)}><StateLine line={line} detailed />{line.detail.length > 0 && <ul {...stylex.props(styles.notes)}>{line.detail.map((note, index) => <li key={index}>{note}</li>)}</ul>}</dd>
  </React.Fragment>)}</dl><section aria-label="Execution identity" {...stylex.props(styles.identity)}><strong>Execution identity</strong><ul {...stylex.props(styles.notes)}>{model.identity.map((note, index) => <li key={index}>{note}</li>)}</ul></section></div>
}
export const LiveState = React.memo(function LiveState({ snapshot, now, label, variant = 'row', style }: LiveStateProps) {
  const model = liveStateModel(snapshot, now)
  const primary = model.primary
  return <div role="group" data-testid="live-state" data-variant={variant} data-primary-axis={primary?.axis ?? 'none'} aria-label={`${label} diagnostics`} {...stylex.props(styles.root, variant === 'header' && styles.header, style)}>
    {variant === 'row' ? <span data-testid="live-state-primary" {...stylex.props(styles.summary)}>{primary !== undefined ? <><SidebarStatus status={primary.glyph} statusLabel={primary.text} freshness={primary.stale ? 'stale' : 'live'} iconOnly /><StateLine line={primary} /></> : <span>Diagnostics aren't reported</span>}</span> : <div data-testid="live-state-header-axes" {...stylex.props(styles.axes)}>{model.lines.filter(line => line.reported).map(line => <div key={line.axis} {...stylex.props(styles.headerAxis)}><span {...stylex.props(styles.label)}>{line.label}</span><StateLine line={line} /></div>)}{model.lines.every(line => !line.reported) && <span>Diagnostics aren't reported</span>}</div>}
    <DialogTrigger><Button aria-label={`Details: full diagnostics for ${label}`} {...stylex.props(styles.button)}>Details</Button><Popover placement="bottom end" {...stylex.props(styles.popover)}><Dialog aria-label={`Full diagnostics for ${label}`} {...stylex.props(styles.dialog)}>{({ close }) => <><div {...stylex.props(styles.detailHeading)}><Heading slot="title" {...stylex.props(styles.heading)}>Full diagnostics</Heading><Button aria-label="Close diagnostics" onPress={close} {...stylex.props(styles.button)}>Close</Button></div><LiveStateDetails model={model} /></>}</Dialog></Popover></DialogTrigger>
  </div>
})
const styles = stylex.create({
  root: { display: 'flex', alignItems: 'center', gap: s.sm, minWidth: 0, width: '100%', height: g.toolRow, fontFamily: t.fontSans, fontSize: t.denseSize, lineHeight: t.metaLeading, color: ink.sidebarFgMuted },
  summary: { display: 'flex', alignItems: 'center', gap: s.xs, minWidth: 0, flex: '1 1 0', height: g.toolRow },
  line: { display: 'inline-flex', alignItems: 'center', gap: s.xs, minWidth: 0, maxWidth: '100%' },
  detailLine: { display: 'flex', flexWrap: 'wrap' }, detailText: { whiteSpace: 'normal', overflow: 'visible', overflowWrap: 'anywhere' },
  text: { overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', minWidth: 0 },
  timer: { display: 'inline-block', width: '17ch', flexShrink: 0, textAlign: 'right', fontFamily: t.fontMono, fontVariantNumeric: 'tabular-nums', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' },
  stale: { color: ink.fgMuted }, danger: { color: tone.dangerFg }, attention: { color: tone.attention },
  header: { height: 'auto', minHeight: g.toolRow, alignItems: 'flex-start', paddingBlock: s.md, paddingInline: s.lg, boxSizing: 'border-box', backgroundColor: surface.canvas, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border },
  axes: { display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(min(100%, 280px), 1fr))', gap: s.md, flex: '1 1 0', minWidth: 0 },
  headerAxis: { display: 'flex', flexDirection: 'column', gap: s.xs2, minWidth: 0 },
  label: { color: ink.fgMuted, fontSize: t.denseSize, fontWeight: t.weightMedium },
  button: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', height: g.toolRow, flexShrink: 0, paddingInline: s.xs, paddingBlock: s.zero, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgSoft, fontFamily: t.fontSans, fontSize: t.denseSize, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  popover: { width: 'min(580px, calc(100vw - 32px))', maxHeight: 'min(640px, calc(100vh - 32px))', overflowY: 'auto', padding: s.lg, boxSizing: 'border-box', backgroundColor: surface.raised, color: ink.fg, borderRadius: r.sm, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong },
  dialog: { outline: 'none', fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading },
  detailHeading: { display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: s.md }, heading: { margin: s.zero, fontSize: t.uiSize, fontWeight: t.weightMedium },
  details: { display: 'grid', gridTemplateColumns: '100px minmax(0, 1fr)', columnGap: s.md, rowGap: s.lg, marginBlock: s.lg }, value: { margin: s.zero, minWidth: 0, overflowWrap: 'anywhere' },
  notes: { listStyle: 'none', padding: s.zero, marginBlock: s.xs, color: ink.fgMuted, overflowWrap: 'anywhere' }, identity: { paddingTop: s.md, borderTopWidth: g.hairline, borderTopStyle: 'solid', borderTopColor: border.border },
})
