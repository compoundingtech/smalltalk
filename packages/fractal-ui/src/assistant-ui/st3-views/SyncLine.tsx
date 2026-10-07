// The host owns decoded facts and the clock.
import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button, Dialog, DialogTrigger, Popover, Tooltip, TooltipTrigger } from 'react-aria-components'
import { syncLine, type SyncLineInput } from './sync-line'
import { accentVars as accent, borderVars as border, geometryVars as g, motionVars as m, radiusVars as r, spaceVars as s, statusVars as status, surfaceVars as surface, textVars as ink, typeVars as t } from '../composition-tokens.stylex'

/** The host owns the clock and decodes its own transport into the portable tagged union. */
export interface SyncLineProps extends SyncLineInput { readonly onRetry?: () => void; readonly compact?: boolean }
const pulse = stylex.keyframes({ from: { opacity: 0.5 }, to: { opacity: 1 } })
/** Always occupies its existing 24px slot; Live and Evicted leave the slot empty, never add a band. */
export const SyncLine = React.memo(function SyncLine({ status: observation, label, now, observedAt, gateway, socket, onRetry, compact = false }: SyncLineProps) {
  const line = syncLine({ status: observation, label, now, observedAt, gateway, socket })
  const failed = observation._tag === 'Failed'
  return <div data-testid="sync-line" data-sync-state={observation._tag} aria-busy={observation._tag !== 'Live' && !failed} {...stylex.props(styles.root, compact && styles.compact)}>
    <TooltipTrigger delay={150}><Button aria-label={line?.text ?? `${label} synchronized`} isDisabled={line === undefined} {...stylex.props(styles.textButton)}>
      <span role="status" aria-live="polite" aria-label={line?.announce ?? ''} {...stylex.props(styles.status)}><span aria-hidden="true" {...stylex.props(styles.dot, line?.tone === 'warning' && styles.warningDot, line?.tone === 'error' && styles.errorDot, line?.animate && styles.pulsing, line === undefined && styles.hidden)} /><span aria-hidden="true" {...stylex.props(styles.text, line?.tone === 'warning' && styles.warning, line?.tone === 'error' && styles.error)}>{line?.text ?? ''}</span></span>
    </Button>{line !== undefined && <Tooltip {...stylex.props(styles.tooltip)}>{line.text}</Tooltip>}</TooltipTrigger>
    <div {...stylex.props(styles.actions, !failed && styles.hidden)}>
      <Button data-command-id="sync.retry" aria-label={`Retry loading ${label}`} isDisabled={!failed || onRetry === undefined} onPress={onRetry} {...stylex.props(styles.button)}>Retry</Button>
      <DialogTrigger><Button aria-label={`Sync details for ${label}`} isDisabled={!failed} {...stylex.props(styles.button)}>Details</Button><Popover {...stylex.props(styles.popover)}><Dialog aria-label={`Sync details for ${label}`} {...stylex.props(styles.dialog)}>{failed && <><strong>{observation.code}</strong><p {...stylex.props(styles.detail)}>{observation.message}</p></>}</Dialog></Popover></DialogTrigger>
    </div>
  </div>
})
const styles = stylex.create({
  root: { display: 'flex', alignItems: 'center', gap: s.xs, width: '100%', minWidth: 0, height: g.toolRow, minHeight: g.toolRow, maxHeight: g.toolRow, flex: '1 1 0', boxSizing: 'border-box', color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, fontVariantNumeric: 'tabular-nums' }, compact: { maxWidth: g.tooltipMax },
  textButton: { display: 'flex', alignItems: 'center', flex: '1 1 0', minWidth: 0, height: g.toolRow, padding: s.zero, borderWidth: 0, backgroundColor: surface.transparent, color: 'inherit', fontFamily: 'inherit', fontSize: 'inherit', textAlign: 'left', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  status: { display: 'flex', alignItems: 'center', gap: s.sm, width: '100%', minWidth: 0, height: t.metaLeading }, text: { display: 'block', flex: '1 1 0', minWidth: 0, height: t.metaLeading, overflow: 'hidden', whiteSpace: 'nowrap', textOverflow: 'ellipsis' }, dot: { width: g.statusDot, height: g.statusDot, flexShrink: 0, borderRadius: r.full, backgroundColor: ink.fgMuted }, warningDot: { backgroundColor: status.attention }, errorDot: { backgroundColor: status.dangerFg }, warning: { color: ink.fgSoft }, error: { color: status.dangerFg }, pulsing: { animationName: pulse, animationDuration: m.pulse, animationIterationCount: 'infinite', animationDirection: 'alternate', '@media (prefers-reduced-motion: reduce)': { animationName: 'none' } }, hidden: { visibility: 'hidden' },
  actions: { display: 'flex', alignItems: 'center', gap: s.xs, flexShrink: 0, width: `calc(${g.controlSm} * 4)` },
  button: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', height: g.toolRow, minHeight: g.toolRow, paddingInline: s.xs, paddingBlock: 0, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.denseSize, lineHeight: t.metaLeading, whiteSpace: 'nowrap', cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  tooltip: { maxWidth: g.tooltipMax, padding: s.md, borderRadius: r.sm, backgroundColor: surface.raised, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong }, popover: { maxWidth: g.tooltipMax, backgroundColor: surface.raised, color: ink.fg, borderRadius: r.sm, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, padding: s.md }, dialog: { outline: 'none', fontFamily: t.fontSans, fontSize: t.metaSize }, detail: { margin: 0, marginTop: s.md, overflowWrap: 'anywhere' },
})
