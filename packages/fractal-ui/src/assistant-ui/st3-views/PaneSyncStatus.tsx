import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button, Dialog, Heading, Popover, Tooltip, TooltipTrigger, VisuallyHidden } from 'react-aria-components'
import { syncLine, canRetrySyncFailure, syncFailureRecovery } from './sync-line'
import type { SyncStatus } from './sync-status'
import { SyncFailureDetails } from './SyncLine'
import { accentVars as accent, borderVars as border, geometryVars as g, radiusVars as r, spaceVars as s, statusVars as status, surfaceVars as surface, textVars as ink, typeVars as t } from '../composition-tokens.stylex'

/** A loading glyph only appears once the observation has lasted this long; fast loads show nothing. */
export const loadingGlyphDelayMs = 300
export interface PaneSyncStatusProps { readonly status: SyncStatus; readonly label: string; readonly now: number; readonly observedAt: number; readonly gateway?: string; readonly detailsOpen: boolean; readonly onDetailsOpenChange: (open: boolean) => void; readonly onRetry: () => void }
type Phase = 'live' | 'loading' | 'failed'
const phaseOf = (observation: SyncStatus): Phase => observation._tag === 'Failed' ? 'failed' : observation._tag === 'Live' || (observation._tag === 'Stale' && observation.reason._tag === 'Evicted') ? 'live' : 'loading'
const sentence = (text: string) => `${text.charAt(0).toUpperCase()}${text.slice(1)}`

/**
 * The one sync status of a pane, rendered inside the pane's fixed-height header row.
 * Live renders nothing; loading is a muted glyph after a delay; failure is a glyph, a status
 * word that never truncates and Details. Retry belongs to the body or its Details popover, never the header.
 */
export const PaneSyncStatus = React.memo(function PaneSyncStatus({ status: observation, label, now, observedAt, gateway, detailsOpen, onDetailsOpenChange: setDetailsOpen, onRetry }: PaneSyncStatusProps) {
  const phase = phaseOf(observation)
  const line = syncLine({ status: observation, label, now, observedAt, gateway })
  const failure = observation._tag === 'Failed' ? observation.cause : undefined
  const retryable = failure !== undefined && canRetrySyncFailure(failure)
  const recovery = failure === undefined ? undefined : syncFailureRecovery(failure)
  const failureText = failure === undefined || failure._tag === 'Unknown' ? undefined : line?.text ?? `Couldn't load ${label}`
  const [glyphShown, setGlyphShown] = React.useState(false)
  React.useEffect(() => {
    setGlyphShown(false)
    if (phase !== 'loading') return
    const timer = setTimeout(() => setGlyphShown(true), loadingGlyphDelayMs)
    return () => clearTimeout(timer)
  }, [phase])
  // Only failure and recovery speak; each transition replaces the region's content exactly once.
  const [announcement, setAnnouncement] = React.useState<{ readonly seq: number; readonly text: string }>()
  const failedSinceLive = React.useRef(false)
  const failureKey = failure === undefined ? undefined : `${failureText ?? 'Load failed'}. ${recovery}`
  React.useEffect(() => {
    if (failureKey !== undefined) { failedSinceLive.current = true; setAnnouncement(previous => ({ seq: (previous?.seq ?? 0) + 1, text: failureKey })) }
    else if (phase === 'live' && failedSinceLive.current) { failedSinceLive.current = false; setAnnouncement(previous => ({ seq: (previous?.seq ?? 0) + 1, text: `${sentence(label)} loaded.` })) }
  }, [failureKey, phase, label])
  const [anchor, setAnchor] = React.useState<'word' | 'details'>('details')
  const wordTrigger = React.useRef<HTMLButtonElement>(null)
  const detailsTrigger = React.useRef<HTMLButtonElement>(null)
  React.useEffect(() => { if (failure === undefined) setDetailsOpen(false) }, [failure,setDetailsOpen])
  const headingId = React.useId()
  const textId = React.useId()
  const openDetails = (from: 'word' | 'details') => { setAnchor(from); setDetailsOpen(true) }
  // The failed controls are always laid out (hidden and inert unless failed), so a phase change never resizes the header's action group.
  const reserved = failure === undefined
  return <div data-pane-sync-status data-sync-state={observation._tag} data-pane-sync-phase={phase} {...stylex.props(styles.root)}>
    {phase === 'loading' && <span data-pane-sync-glyph={glyphShown ? 'shown' : 'pending'} role="img" aria-label={line?.text ?? `Loading ${label}`} aria-hidden={!glyphShown || undefined} {...stylex.props(styles.glyphBox, !glyphShown && styles.pending)}><span {...stylex.props(styles.dot)} /></span>}
    <span data-pane-sync-controls={reserved ? 'reserved' : 'shown'} aria-hidden={reserved || undefined} inert={reserved} {...stylex.props(styles.controls, reserved && styles.pending)}>
      {reserved ? <span {...stylex.props(styles.word, styles.wordStatic)}><span {...stylex.props(styles.dot)} />Load failed</span> : <span data-pane-sync-word {...stylex.props(styles.word, styles.wordStatic)}><span {...stylex.props(styles.dot, styles.dangerDot)} />Load failed</span>}
      {reserved ? <span {...stylex.props(styles.word, styles.wordTrigger)}><span {...stylex.props(styles.dot)} /><span data-pane-sync-word-label {...stylex.props(styles.wordLabel)}>Load failed</span></span> : <TooltipTrigger delay={150}><Button ref={wordTrigger} data-pane-sync-word-trigger aria-label={retryable ? 'Load failed. Open details to retry.' : 'Load failed. Open details for recovery information.'} aria-haspopup="dialog" aria-expanded={detailsOpen && anchor === 'word'} onPress={() => openDetails('word')} {...stylex.props(styles.word, styles.wordTrigger)}><span {...stylex.props(styles.dot, styles.dangerDot)} /><span data-pane-sync-word-label {...stylex.props(styles.wordLabel)}>Load failed</span></Button><Tooltip {...stylex.props(styles.tooltip)}>Load failed</Tooltip></TooltipTrigger>}
      {reserved ? <span {...stylex.props(styles.action, styles.detailsAction)}><span {...stylex.props(styles.actionIcon)}>⋯</span><span {...stylex.props(styles.actionLabel)}>Details</span></span> : <Button ref={detailsTrigger} data-pane-sync-details-trigger aria-label={`Details for ${label}`} aria-expanded={detailsOpen} onPress={() => openDetails('details')} {...stylex.props(styles.action, styles.detailsAction)}><span aria-hidden="true" {...stylex.props(styles.actionIcon)}>⋯</span><span aria-hidden="true" {...stylex.props(styles.actionLabel)}>Details</span></Button>}
    </span>
    {failure !== undefined && <>
      <Popover triggerRef={anchor === 'word' ? wordTrigger : detailsTrigger} isOpen={detailsOpen} onOpenChange={setDetailsOpen} placement="bottom end" {...stylex.props(styles.popover)}>
        <Dialog data-pane-sync-details aria-describedby={failureText === undefined ? undefined : textId} {...stylex.props(styles.dialog)}>
          <Heading slot="title" id={headingId} {...stylex.props(styles.heading)}>Load failed</Heading>
          {failureText !== undefined && <><div id={textId}><p {...stylex.props(styles.paragraph, styles.failureText)}>{failureText}.</p><p {...stylex.props(styles.paragraph)}>{recovery}</p></div>
          <div {...stylex.props(styles.cause)}><SyncFailureDetails cause={failure} />{gateway !== undefined && <p {...stylex.props(styles.paragraph)}>Gateway: {gateway}</p>}</div></>}
          {retryable && <div {...stylex.props(styles.dialogActions)}><Button data-command-id="sync.retry.details" onPress={() => { setDetailsOpen(false); onRetry() }} {...stylex.props(styles.primary)}>Retry</Button></div>}
        </Dialog>
      </Popover>
    </>}
    <VisuallyHidden><span role="status" aria-live="polite" aria-atomic="true" data-pane-sync-announcer>{announcement !== undefined && <span key={announcement.seq}>{announcement.text}</span>}</span></VisuallyHidden>
  </div>
})

// Wider folding uses padded header content; glyph-only recovery uses the actual mounted pane width.
const narrow = '@container pane-header (width < 360px)'
const folded = '@container pane-header (width < 200px)'
const glyphOnly = '@container pane-mount (width < 160px)'
const glyphFallback = '@container pane-header (width < 160px)'
const styles = stylex.create({
  // position:relative anchors the loading glyph over the reserved controls' inline end.
  root: { position: 'relative', display: 'flex', alignItems: 'center', gap: s.xs2, height: g.controlSm, flexShrink: 0, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.denseSize, lineHeight: t.metaLeading, whiteSpace: 'nowrap' },
  controls: { display: 'flex', alignItems: 'center', gap: s.xs2, flexShrink: 0 },
  glyphBox: { position: 'absolute', insetInlineEnd: s.zero, top: '50%', transform: 'translateY(-50%)', display: 'inline-flex', alignItems: 'center', justifyContent: 'center', width: g.icon, height: g.icon, flexShrink: 0 },
  pending: { visibility: 'hidden' },
  dot: { width: g.statusDot, height: g.statusDot, flexShrink: 0, borderRadius: r.full, backgroundColor: ink.fgMuted },
  dangerDot: { backgroundColor: status.dangerFg },
  // The status word never shrinks; the pane title gives way first.
  word: { display: 'inline-flex', alignItems: 'center', gap: s.xs, flexShrink: 0, height: g.controlSm, color: status.dangerFg, fontFamily: t.fontSans, fontSize: t.denseSize, lineHeight: t.metaLeading, whiteSpace: 'nowrap' },
  wordStatic: { display: { default: 'inline-flex', [folded]: 'none' }, paddingInline: s.xs2 },
  // Below 200px header content the word opens Details; below 160px mounted width it is the sole status action.
  wordTrigger: { display: { default: 'none', [folded]: 'inline-flex' }, boxSizing: 'border-box', width: { default: 'auto', [glyphOnly]: g.controlSm, [glyphFallback]: g.controlSm }, justifyContent: 'center', gap: { default: s.xs, [glyphOnly]: s.zero, [glyphFallback]: s.zero }, paddingInline: { default: s.xs2, [glyphOnly]: s.zero, [glyphFallback]: s.zero }, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  // The padded-header glyph fallback still applies when the status word cannot fit.
  wordLabel: { display: { default: 'inline', [glyphOnly]: 'none', [glyphFallback]: 'none' } },
  action: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', flexShrink: 0, boxSizing: 'border-box', minWidth: g.controlSm, height: g.controlSm, paddingInline: { default: s.xs, [narrow]: s.zero }, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.denseSize, lineHeight: t.metaLeading, whiteSpace: 'nowrap', cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover, color: ink.fg }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  detailsAction: { display: { default: 'inline-flex', [folded]: 'none' } },
  actionIcon: { display: { default: 'none', [narrow]: 'inline' } },
  actionLabel: { display: { default: 'inline', [narrow]: 'none' } },
  // max-content: anywhere-wrapping text would otherwise shrink-to-fit the popover to one word per line.
  popover: { width: 'max-content', maxWidth: g.tooltipMax, boxSizing: 'border-box', backgroundColor: surface.raised, color: ink.fg, borderRadius: r.sm, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, padding: s.md },
  tooltip: { maxWidth: g.tooltipMax, paddingInline: s.md, paddingBlock: s.sm, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.sm, backgroundColor: surface.raised, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading },
  dialog: { display: 'flex', flexDirection: 'column', gap: s.xs, outline: 'none', fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, whiteSpace: 'normal' },
  heading: { margin: s.zero, color: status.dangerFg, fontSize: t.metaSize, lineHeight: t.metaLeading, fontWeight: t.weightSemibold },
  paragraph: { margin: s.zero, overflowWrap: 'anywhere' },
  failureText: { color: ink.fg },
  cause: { color: ink.fgMuted, overflowWrap: 'anywhere' },
  dialogActions: { display: 'flex', gap: s.xs, paddingTop: s.xs },
  primary: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', minHeight: g.controlSm, paddingInline: s.sm, borderWidth: 0, borderRadius: r.control, backgroundColor: accent.primary, color: accent.onPrimary, fontFamily: t.fontSans, fontSize: t.denseSize, lineHeight: t.metaLeading, cursor: 'pointer', ':hover': { backgroundColor: accent.primaryHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.onPrimary } },
})
