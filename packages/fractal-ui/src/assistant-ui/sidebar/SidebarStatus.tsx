// Explicit-clock presentation only.
import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { geometryVars as g, motionVars as m, spaceVars as s, statusVars as tone, textVars as ink, typeVars as t } from '../composition-tokens.stylex'
import { compactTime, statuses, type AgentStatus } from './model'

export type GlyphVariant = 'SG-1' | 'SG-2' | 'SG-3'
export type DiscRefinement = 'G1' | 'G2' | 'G3'
export const glyphDescriptions: Record<GlyphVariant, string> = { 'SG-1': '8px discs in a fixed16px glyph column; refinements add running ring or canonical shape cues', 'SG-2': 'Quiet colored dot with the full canonical status word', 'SG-3': '16px stroke-only canonical status shapes' }
export interface SidebarStatusProps {
  readonly status: AgentStatus
  readonly statusLabel?: string
  readonly variant?: GlyphVariant
  readonly discRefinement?: DiscRefinement
  readonly statusSince?: number
  readonly now?: number
  readonly freshness?: 'live' | 'stale' | 'unobserved'
  readonly iconOnly?: boolean
}
const paths: Record<AgentStatus, React.ReactNode> = {
  working: <><circle cx="10" cy="10" r="7" opacity=".4" /><path d="M10 3a7 7 0 0 1 7 7" /></>,
  waiting: <><circle cx="10" cy="10" r="7" /><path d="M7.7 7.5a2.3 2.3 0 1 1 3.9 1.7c-1 .6-1.6 1.1-1.6 2.3M10 14h.01" /></>,
  idle: <path d="M16.5 11.5A7 7 0 0 1 8.5 3.5a7 7 0 1 0 8 8Z" />,
  pending: <path d="M6 3h8M6 17h8M7 3v3l6 8v3M13 3v3l-6 8v3" />,
  stale: <><circle cx="10" cy="10" r="7" strokeDasharray="2 2" /><path d="M10 6v4l3 2" /></>,
  offline: <path d="M3 8a11 11 0 0 1 14 0M6 11a6 6 0 0 1 8 0M9 14a2 2 0 0 1 2 0M3 3l14 14" />,
  ended: <><circle cx="10" cy="10" r="7" /><rect x="7" y="7" width="6" height="6" rx=".5" fill="currentColor" stroke="none" /></>,
  suspended: <><circle cx="10" cy="10" r="7" /><path d="M8 7v6M12 7v6" /></>,
  retired: <><rect x="3" y="4" width="14" height="3" rx="1" /><path d="M4 7v9h12V7M8 10h4" /></>,
  unobserved: <><circle cx="10" cy="10" r="7" strokeDasharray="2 3" /><path d="M10 7v4M10 14h.01" /></>,
}
/** Shapes never collapse distinct states into an attention or completion fixture state. */
export function SidebarStatus({ status, statusLabel = statuses[status].label, variant = 'SG-1', discRefinement = 'G1', statusSince, now, freshness = 'unobserved', iconOnly = false }: SidebarStatusProps) {
  const boundary = statusSince === undefined ? 'Status boundary unavailable' : `Observed status since ${new Date(statusSince).toISOString()}`
  const elapsed = statusSince !== undefined && now !== undefined ? compactTime({ at: statusSince, now }) : undefined
  const runningRing = variant === 'SG-1' && discRefinement === 'G2' && status === 'working'
  const shapeCue = variant === 'SG-1' && discRefinement === 'G3' && status !== 'working' && status !== 'idle'
  return <span title={`${statusLabel}. ${boundary}. Observation ${freshness}.`} {...stylex.props(styles.root, status === 'working' && styles.working, status === 'waiting' && styles.waiting, status === 'stale' && styles.stale)}>
    <svg data-disc-refinement={variant === 'SG-1' ? discRefinement : undefined} role="img" aria-label={`${statusLabel}; ${boundary}; ${freshness} observation`} viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" {...stylex.props(styles.icon, runningRing && styles.iconRing, (runningRing || (status === 'working' && variant === 'SG-3')) && styles.spin)}>{variant === 'SG-2' ? <circle cx="10" cy="10" r="4" fill="currentColor" stroke="none" /> : variant === 'SG-3' ? paths[status] : <><circle cx="10" cy="10" r="5" fill="currentColor" stroke="none" opacity={shapeCue ? '.25' : undefined} />{runningRing && <><circle cx="10" cy="10" r="7.5" opacity=".3" /><path d="M10 2.5a7.5 7.5 0 0 1 7.5 7.5" /></>}{shapeCue && paths[status]}</>}</svg>
    {variant === 'SG-2' && !iconOnly && <span>{statusLabel}</span>}
    {elapsed !== undefined && <time dateTime={new Date(statusSince!).toISOString()} aria-label={`Elapsed in ${statusLabel}: ${elapsed}`}>{elapsed}</time>}
  </span>
}
const styles = stylex.create({
  root: { display: 'inline-flex', alignItems: 'center', gap: s.xs, color: ink.sidebarFgMuted, fontSize: t.denseSize, lineHeight: t.metaLeading, fontVariantNumeric: 'tabular-nums', whiteSpace: 'nowrap', flexShrink: 0 },
  icon: { width: g.icon, height: g.icon, flexShrink: 0 }, iconRing: { width: g.glyphRing, height: g.glyphRing },
  working: { color: tone.runningFg }, waiting: { color: tone.attention }, stale: { color: tone.dangerFg },
  spin: { animationName: stylex.keyframes({ to: { transform: 'rotate(360deg)' } }), animationDuration: m.spin, animationTimingFunction: m.linear, animationIterationCount: 'infinite', '@media (prefers-reduced-motion: reduce)': { animationName: 'none' } },
})
