import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button, Tooltip, TooltipTrigger } from 'react-aria-components'
import { surfaceVars as surface, textVars as ink, borderVars as border, accentVars as accent, statusVars as status, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g } from '../composition-tokens.stylex'
import { Icon } from '../composition/Icons'
import { ErrorOverlay, useErrorOverlaySurface } from '../composition/ErrorOverlay'
import { HighlightedSource } from '../composition/Markdown'
import type { WorkLogCall, WorkLogTurn } from './work-log'
import { formatWorkDuration, workLogOutputLanguage } from './work-log'

/** `expanded` is true only after the reader opens the row; previews and read-only rows stay collapsed. */
export type WorkLogCallDetailRenderer = (call: WorkLogCall, state: { readonly expanded: boolean }) => React.ReactNode
const CallDetailRenderer = React.createContext<WorkLogCallDetailRenderer | undefined>(undefined)
const CallPresentation = React.createContext({ preview: false, interactive: true })
const timeFormat = new Intl.DateTimeFormat(undefined, { hour: 'numeric', minute: '2-digit' })
/** Host-owned facts and actions; incomplete, running and failed work never folds. */
export function WorkLogV1({ turn, listStyle, renderCallDetail, previewCallDetail = false, interactiveCalls = true, hideLiveRow = false, expandedBody, ariaLabel = 'Work log', onRetry, onOpenOutput }: {
  readonly turn: WorkLogTurn
  readonly listStyle?: stylex.StyleXStyles
  readonly renderCallDetail?: WorkLogCallDetailRenderer
  readonly previewCallDetail?: boolean
  readonly interactiveCalls?: boolean
  readonly hideLiveRow?: boolean
  readonly expandedBody?: React.ReactNode
  readonly ariaLabel?: string
  readonly onRetry?: () => void
  readonly onOpenOutput?: (call: WorkLogCall) => void
}) {
  const overlayLayer = useErrorOverlaySurface()
  const settled = !turn.running && !turn.failed && !turn.interrupted && turn.foldable !== false
  const durationLabel = formatWorkDuration(turn.durationMs)
  const [open, setOpen] = React.useState(false)
  const failedCall = turn.calls.find(call => call.status === 'error')
  const failed = turn.failed || failedCall !== undefined
  const workLabel = turn.running ? 'Working' : settled ? `Worked${durationLabel ? ` for ${durationLabel}` : ''}` : failed ? 'Run failed' : turn.interrupted ? 'Run interrupted' : 'Work · completion unverified'
  const summary = [workLabel, settled && turn.commands ? `${turn.commands} ${turn.commands === 1 ? 'command' : 'commands'}` : undefined, settled && turn.changedFiles ? `${turn.changedFiles} ${turn.changedFiles === 1 ? 'file changed' : 'files changed'}` : undefined].filter(value => value !== undefined).join(' · ')
  const started = turn.running ? NaN : Date.parse(turn.startedAt ?? '')
  const failedCommand = failedCall?.kind === 'run' ? failedCall.argsSummary ?? failedCall.title : undefined
  const detail = [failedCommand, failedCall?.detail, turn.failureNote].filter((value, index, values) => value !== undefined && values.indexOf(value) === index).join('\n') || undefined
  const summaryContent = <><span {...stylex.props(styles.summaryLabel)}>{summary}</span>{Number.isFinite(started) && <time dateTime={new Date(started).toISOString()} {...stylex.props(styles.time)}>{timeFormat.format(started)}</time>}</>
  return <CallDetailRenderer.Provider value={renderCallDetail}><CallPresentation.Provider value={{ preview: previewCallDetail, interactive: interactiveCalls }}><section aria-label={ariaLabel} data-testid="work-log">
    {turn.running && !hideLiveRow && <div data-testid="live-work" role="status" aria-label="Response in progress" {...stylex.props(styles.live)}><Icon name="spinner" spinning /><span>Working</span></div>}
    {interactiveCalls ? <Button isDisabled={!settled} aria-expanded={!settled || open} onPress={() => setOpen(value => !value)} {...stylex.props(styles.summary, failed && styles.failureInk)}><Icon name={!settled || open ? 'chevron-down' : 'chevron-right'} size={12} />{summaryContent}</Button> : <div {...stylex.props(styles.summary, styles.staticCall, failed && styles.failureInk)}>{summaryContent}</div>}
    {(!interactiveCalls || !settled || open) && <><div {...stylex.props(styles.list, listStyle)}>{turn.calls.map(call => <TimelineCall key={call.id} call={call} />)}</div>{expandedBody}<hr data-testid="work-log-divider" {...stylex.props(styles.divider)} /></>}
    {failed && (overlayLayer !== null ? <ErrorOverlay id={`work-failure-${turn.calls[0]?.id ?? 'turn'}`} title={failedCommand === undefined ? 'Run failed.' : 'Command did not complete'} detail={detail} onRetry={onRetry} onOpenOutput={failedCall === undefined || onOpenOutput === undefined ? undefined : () => onOpenOutput(failedCall)} /> : <div role="alert" {...stylex.props(styles.promoted, styles.failed)}><Icon name="x" size={14} /><span>{failedCall?.detail ?? turn.failureNote ?? 'Run failed.'}</span></div>)}
    {turn.interrupted && <div {...stylex.props(styles.promoted)}><Icon name="stop" size={14} /><span>Run interrupted.</span></div>}
  </section></CallPresentation.Provider></CallDetailRenderer.Provider>
}
function TimelineCall({ call }: { call: WorkLogCall }) {
  const [open, setOpen] = React.useState(false)
  const renderDetail = React.useContext(CallDetailRenderer)
  const { preview, interactive } = React.useContext(CallPresentation)
  const hasOutput = call.detail !== undefined && call.detail.length > 0
  const displayed = hasOutput && (!interactive || open || preview && call.status !== 'error')
  const result = call.detail ?? (call.status === 'running' ? 'Running…' : 'No output')
  const label = call.argsSummary === undefined ? call.title : `${call.title} ${call.argsSummary}`
  const start = Date.parse(call.startedAt), end = Date.parse(call.endedAt ?? '')
  const duration = Number.isFinite(start) && Number.isFinite(end) && end >= start ? formatWorkDuration(end - start) || '<1s' : undefined
  const state = call.status === 'running' ? 'Running' : call.status === 'error' ? 'Failed' : call.status === 'interrupted' ? 'Interrupted' : 'OK'
  const row = <><Icon name={call.status === 'running' ? 'spinner' : call.status === 'error' ? 'alert' : call.status === 'success' ? 'check' : 'stop'} spinning={call.status === 'running'} size={14} /><Icon name={call.kind === 'read' ? 'search' : call.kind === 'run' ? 'drawer' : 'pencil'} size={14} /><span title={label} {...stylex.props(styles.callLabel)}>{call.kind === 'run' ? <code {...stylex.props(styles.mono)}><HighlightedSource code={label} language="bash" /></code> : <>{call.title}{call.argsSummary === undefined ? null : <>{' '}<code {...stylex.props(styles.mono)}>{call.argsSummary}</code></>}</>}</span>{hasOutput && interactive && <span data-row-disclosure={displayed ? 'open' : 'closed'} {...stylex.props(styles.disclosure)}><Icon name={displayed ? 'chevron-down' : 'chevron-right'} size={12} /></span>}<span {...stylex.props(styles.callState)}>{state}{!hasOutput && call.status !== 'running' ? ' · No output' : ''}{duration === undefined ? '' : ` · ${duration}`}</span></>
  return <div data-tool-status={call.status}>{hasOutput && interactive ? <TooltipTrigger delay={150}><Button aria-expanded={displayed} onPress={() => setOpen(value => !value)} {...stylex.props(styles.call, open && styles.expanded, call.status === 'error' && styles.failureInk)}>{row}</Button><Tooltip {...stylex.props(styles.hover)}><div>{label}</div><div>{result}</div></Tooltip></TooltipTrigger> : <div {...stylex.props(styles.call, styles.staticCall, call.status === 'error' && styles.failureInk)}>{row}</div>}{displayed && (renderDetail === undefined ? <>{open && call.rawInput !== undefined && <pre data-testid="tool-raw-input" {...stylex.props(styles.output)}><code>{call.rawInput}</code></pre>}<pre data-testid="work-call-output" {...stylex.props(styles.output)}><code><HighlightedSource code={result} language={workLogOutputLanguage(call)} /></code></pre></> : renderDetail(call, { expanded: open }))}</div>
}
const styles = stylex.create({
  summary: { display: 'flex', alignItems: 'center', gap: s.sm, width: '100%', minHeight: g.toolRow, borderWidth: 0, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border, backgroundColor: surface.transparent, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.uiSize, padding: 0, cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  summaryLabel: { minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  list: { maxHeight: g.workExpandedMax, overflowY: 'auto', overscrollBehavior: 'contain' },
  call: { display: 'flex', alignItems: 'center', gap: s.sm, width: '100%', minHeight: g.toolRow, paddingInline: s.xs, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.uiSize, textAlign: 'left', cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover, color: ink.fg }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  callLabel: { flex: '1 1 auto', minWidth: 0, whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }, callState: { marginLeft: 'auto', flexShrink: 0, color: 'inherit', fontSize: t.metaSize },
  expanded: { backgroundColor: surface.rowActive, color: ink.fg }, staticCall: { cursor: 'default' }, disclosure: { display: 'inline-flex', flexShrink: 0 },
  mono: { fontFamily: t.fontMono, fontSize: t.metaSize },
  output: { marginBlock: 0, paddingInline: s.xxl, paddingBlock: s.xs, color: ink.fgMuted, fontFamily: t.fontMono, fontSize: t.metaSize, lineHeight: t.metaLeading, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' },
  divider: { height: 0, margin: 0, borderWidth: 0, borderTopWidth: g.hairline, borderTopStyle: 'solid', borderTopColor: border.border },
  time: { flexShrink: 0, color: ink.fgMuted, fontSize: t.metaSize, marginLeft: 'auto' },
  live: { minHeight: g.toolRow, display: 'flex', alignItems: 'center', gap: s.md, color: ink.fgMuted, fontSize: t.metaSize },
  promoted: { display: 'flex', alignItems: 'center', gap: s.md, marginTop: s.md, padding: s.md, backgroundColor: surface.washSubtle, borderRadius: r.md, color: ink.fgSoft, fontSize: t.metaSize, lineHeight: t.uiLeading },
  failed: { backgroundColor: status.diffRemovedWash, color: status.dangerFg }, failureInk: { color: status.dangerFg },
  hover: { maxWidth: g.tooltipMax, padding: s.md, borderRadius: r.md, backgroundColor: surface.raised, color: ink.fg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.uiLeading, zIndex: 10 },
})
