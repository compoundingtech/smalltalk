import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button, Tooltip, TooltipTrigger } from 'react-aria-components'
import { surfaceVars as surface, textVars as ink, borderVars as border, accentVars as accent, statusVars as status, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g } from '../composition-tokens.stylex'
import { Icon } from '../composition/Icons'
import { ErrorOverlay, useErrorOverlaySurface } from '../composition/ErrorOverlay'
import type { WorkLogCall, WorkLogTurn } from './work-log'
import { formatWorkDuration } from './work-log'
/** Controlled work log. The host supplies verified facts; there is no demo fallback and no actions. */
export type WorkLogCallDetailRenderer = (call: WorkLogCall) => React.ReactNode
const CallDetailRenderer = React.createContext<WorkLogCallDetailRenderer | undefined>(undefined)
export function WorkLogV1({ turn, listStyle, renderCallDetail }: {
  readonly turn: WorkLogTurn
  readonly listStyle?: stylex.StyleXStyles
  readonly renderCallDetail?: WorkLogCallDetailRenderer
}) {
  const overlayLayer = useErrorOverlaySurface()
  const settled = !turn.running && !turn.failed && !turn.interrupted
  const durationLabel = formatWorkDuration(turn.durationMs)
  const [open, setOpen] = React.useState(false)
  const workLabel = `Worked${durationLabel ? ` for ${durationLabel}` : ''}`
  return <CallDetailRenderer.Provider value={renderCallDetail}><section aria-label="Work log" data-testid="work-log">
    {turn.running ? <div data-testid="live-work" {...stylex.props(styles.live)}><Icon name="spinner" spinning /><span>Working</span></div> : <>
      <Button isDisabled={!settled} aria-expanded={!settled || open} onPress={() => setOpen(value => !value)} {...stylex.props(styles.summary)}><span>{settled ? workLabel : turn.failed ? 'Run failed' : 'Run stopped'}</span><Icon name={!settled || open ? 'chevron-down' : 'chevron-right'} size={12} /></Button>
      {(!settled || open) && <CallList calls={turn.calls} listStyle={listStyle} />}
    </>}
    {turn.failed && turn.failureNote !== undefined ? (overlayLayer !== null
      ? <ErrorOverlay id={`work-failure-${turn.calls[0]?.id ?? 'turn'}`} title="Run failed." detail={turn.failureNote} />
      : <div role="alert" {...stylex.props(styles.promoted)}><span {...stylex.props(styles.error)}><Icon name="x" size={14} /></span><span>{turn.failureNote}</span></div>) : null}
  </section></CallDetailRenderer.Provider>
}
function CallList({ calls: items, listStyle }: { calls: readonly WorkLogCall[]; listStyle?: stylex.StyleXStyles }) { return <div {...stylex.props(styles.list, listStyle)}>{items.map(call => <TimelineCall key={call.id} call={call} />)}</div> }
function TimelineCall({ call }: { call: WorkLogCall }) {
  const [open, setOpen] = React.useState(false)
  const renderDetail = React.useContext(CallDetailRenderer)
  const result = call.detail ?? call.argsSummary ?? (call.status === 'running' ? 'Running…' : '')
  const label = call.argsSummary === undefined ? call.title : `${call.title} ${call.argsSummary}`
  return <div><TooltipTrigger delay={150}><Button aria-expanded={open} onPress={() => setOpen(value => !value)} {...stylex.props(styles.call)}><span {...stylex.props(styles.dot)} /><Icon name={call.kind === 'read' ? 'search' : call.kind === 'run' ? 'drawer' : 'pencil'} size={14} /><span {...stylex.props(styles.callLabel)}>{label}</span><Icon name={open ? 'chevron-down' : 'chevron-right'} size={12} /></Button><Tooltip {...stylex.props(styles.hover)}><div>{label}</div><div>{result}</div></Tooltip></TooltipTrigger>{open && (renderDetail === undefined ? <div {...stylex.props(styles.output)}>{result}</div> : renderDetail(call))}</div>
}
const styles = stylex.create({
  summary: { display: 'flex', alignItems: 'center', gap: s.sm, width: '100%', minHeight: g.toolRow, borderWidth: 0, borderBottomWidth: 1, borderBottomStyle: 'solid', borderBottomColor: border.border, backgroundColor: surface.transparent, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.uiSize, padding: 0, cursor: 'pointer', ':focus-visible': { outline: `2px solid ${accent.primary}` } },
  list: { maxHeight: g.workExpandedMax, overflowY: 'auto', overscrollBehavior: 'contain' }, call: { display: 'flex', alignItems: 'center', gap: s.sm, width: '100%', minHeight: g.toolRow, paddingInline: s.xs, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.uiSize, textAlign: 'left', cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outline: `2px solid ${accent.primary}` } }, callLabel: { flexGrow: 1, minWidth: 0, overflowWrap: 'anywhere' }, dot: { height: 6, width: 6, borderRadius: '50%', backgroundColor: ink.fgMuted, flexShrink: 0 },
  output: { paddingInline: s.xxl, paddingBlock: s.xs, color: ink.fgMuted, fontFamily: t.fontMono, fontSize: t.metaSize, lineHeight: t.metaLeading },
  live: { minHeight: g.toolRow, display: 'flex', alignItems: 'center', gap: s.md, color: ink.fgMuted, fontSize: t.metaSize }, promoted: { display: 'flex', alignItems: 'center', gap: s.md, marginTop: s.md, padding: s.md, backgroundColor: surface.washSubtle, borderRadius: r.md, color: ink.fgSoft, fontSize: t.metaSize, lineHeight: t.uiLeading }, error: { color: status.dangerFg }, hover: { maxWidth: 360, padding: s.md, borderRadius: r.md, backgroundColor: surface.raised, color: ink.fg, borderWidth: 1, borderStyle: 'solid', borderColor: border.borderStrong, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.uiLeading, zIndex: 10 },
})
