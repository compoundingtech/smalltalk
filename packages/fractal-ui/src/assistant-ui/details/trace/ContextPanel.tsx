import * as stylex from '@stylexjs/stylex'
import type { ReactNode } from 'react'
import { accentVars as accent, borderVars as border, spaceVars as space, statusVars as status, surfaceVars as surface, textVars as text, typeVars as type } from '../../composition-tokens.stylex.ts'
import { baseline, CHART_HEIGHT, CHART_INSET, CHART_WIDTH, clock, linePath, xFor, yFor } from './chart.ts'
import type { ChartScene as AgentTraceScene, ChartLane as Lane } from './adapter.ts'
import { DetailsSkeletonPlot } from '../DetailsSkeleton.tsx'
import { MissingDetailsValue } from '../DetailsUnavailable.tsx'

export interface ContextPanelProps { readonly scene: AgentTraceScene; readonly lane: Lane; readonly compact?: boolean }

/** Formatted text is an inert wrap footprint, never a pending observation. */
export interface ContextPanelFootprint {
  readonly summary: string
  readonly source: string
  readonly axis: readonly [string, string, string]
  readonly compactions: readonly string[]
  readonly estimated: boolean
}

export const contextPanelFootprint = (lane: Lane, compact: boolean): ContextPanelFootprint | undefined => {
  const usesTurns = lane.turnSamples !== undefined && (compact || lane.samples.length === 0)
  const samples = usesTurns ? lane.turnSamples! : lane.samples
  const points = samples.filter(sample => sample.contextPercent !== null)
  if (points.length === 0) return undefined
  const sampleKind = usesTurns ? 'turn' : 'bucket'
  const start = points[0]!.ts
  const end = points.at(-1)!.ts
  const maximum = points.reduce((value, point) => Math.max(value, point.contextPercent!), points[0]!.contextPercent!)
  const firstIndex = samples[0] !== undefined && 'turnIndex' in samples[0] ? samples[0].turnIndex : 1
  const lastSample = samples.at(-1)
  const lastIndex = lastSample !== undefined && 'turnIndex' in lastSample ? lastSample.turnIndex : samples.length
  const cadence = lane.bucketMs === 60_000 ? 'per minute' : lane.bucketMs !== undefined ? `${lane.bucketMs / 1000}s buckets` : 'time buckets'
  return {
    summary: compact ? `${points.length} ${sampleKind} samples` : `Peak ${Math.round(maximum)}%`,
    source: points.every(point => point.isContextEstimated) ? 'estimates' : points.some(point => point.isContextEstimated) ? 'observed + estimates' : 'observed',
    axis: compact
      ? [usesTurns ? `Turn ${firstIndex}` : clock.format(start), usesTurns ? 'per turn · compaction ticks' : `${cadence} · compaction ticks`, usesTurns ? `Turn ${lastIndex}` : clock.format(end)]
      : [clock.format(start), 'UTC · ● measured · ○ estimated', clock.format(end)],
    compactions: compact ? [] : lane.compactions.map(compaction => `${clock.format(compaction.ts)}${compaction.before !== null && compaction.after !== null ? ` · ${Math.round(compaction.before)}% → ${Math.round(compaction.after)}%` : compaction.before !== null ? ` · Before ${Math.round(compaction.before)}%` : compaction.after !== null ? ` · After ${Math.round(compaction.after)}%` : ''}`),
    estimated: points.some(point => point.isContextEstimated),
  }
}

/** Context observations retain their source granularity; buckets are never renamed turns. */
export const ContextPanel = ({ lane, compact = false }: ContextPanelProps): ReactNode => {
  const turnSamples = lane.turnSamples
  const usesTurns = turnSamples !== undefined && (compact || lane.samples.length === 0)
  const samples = usesTurns ? turnSamples : lane.samples
  const sampleKind = usesTurns ? 'turn' : 'bucket'
  const points = samples.flatMap((sample, index) => sample.contextPercent === null ? [] : [{ ts: sample.ts, index: 'turnIndex' in sample ? sample.turnIndex : index + 1, value: sample.contextPercent, estimated: sample.isContextEstimated }])
  if (points.length === 0) return <div {...stylex.props(styles.root)}><span {...stylex.props(styles.note)}>Context series <MissingDetailsValue label="Context series" reason={`No ${sampleKind} context samples retained.`}/></span></div>
  const start = points[0]!.ts
  const end = points.at(-1)!.ts
  const maximumPoint = points.reduce((maximum, point) => point.value > maximum.value ? point : maximum, points[0]!)
  const maximum = 100
  const firstIndex = samples[0] !== undefined && 'turnIndex' in samples[0] ? samples[0].turnIndex : 1
  const lastSample = samples.at(-1)
  const lastIndex = lastSample !== undefined && 'turnIndex' in lastSample ? lastSample.turnIndex : samples.length
  const turnX = (index: number) => firstIndex === lastIndex ? CHART_WIDTH / 2 : CHART_INSET + ((index - firstIndex) / (lastIndex - firstIndex)) * (CHART_WIDTH - CHART_INSET * 2)
  const plot = points.map(point => ({ ...point, x: compact && usesTurns ? turnX(point.index) : xFor(point.ts, start, end), y: yFor(point.value, maximum) }))
  const cadence = lane.bucketMs === 60_000 ? 'per minute' : lane.bucketMs !== undefined ? `${lane.bucketMs / 1000}s buckets` : 'time buckets'
  const axisLabel = usesTurns ? 'per turn · compaction ticks' : `${cadence} · compaction ticks`
  const sourceLabel = points.every(point => point.estimated) ? 'estimates' : points.some(point => point.estimated) ? 'observed + estimates' : 'observed'
  const area = `${linePath(plot)} L${plot.at(-1)!.x.toFixed(1)},${baseline} L${plot[0]!.x.toFixed(1)},${baseline} Z`
  const visibleCompactions = lane.compactions.filter(compaction => compaction.ts >= start && compaction.ts <= end)
  return <div data-chart-domain={compact ? sampleKind : 'time'} data-chart-sample-kind={sampleKind} data-chart-source-count={samples.length} data-chart-samples={points.length} data-chart-compactions={visibleCompactions.length} {...stylex.props(styles.root)}>
    <div {...stylex.props(styles.header)}><span>{compact ? `${points.length} ${sampleKind} samples` : `Peak ${Math.round(maximumPoint.value)}%`}</span><span {...stylex.props(styles.note)}>{sourceLabel}</span></div>
    <svg {...stylex.props(styles.chart, compact && styles.compact)} viewBox={`0 0 ${CHART_WIDTH} ${CHART_HEIGHT}`} preserveAspectRatio="none" role="img" aria-label={`${compact ? usesTurns ? 'Per-turn context sparkline' : 'Bucket context sparkline' : 'Context occupancy area chart'}, filled measured and hollow estimated points, vertical compaction ticks`}>
      <path d={`M${CHART_INSET} ${baseline}H${CHART_WIDTH - CHART_INSET}`} stroke={border.borderStrong} />
      {!compact && <path d={area} fill={accent.primary} fillOpacity={0.12} />}
      <path d={linePath(plot)} fill="none" stroke={accent.primary} strokeWidth={1.5} vectorEffect="non-scaling-stroke" />
      {plot.map((point, index) => <circle key={`${point.ts}-${index}`} cx={point.x} cy={point.y} r={2.5} fill={point.estimated ? surface.canvas : accent.primary} stroke={accent.primary} strokeWidth={1.2} vectorEffect="non-scaling-stroke"><title>{clock.format(point.ts)} · {Math.round(point.value)}% · {point.estimated ? 'estimated' : 'measured'}</title></circle>)}
      {visibleCompactions.map((compaction, index) => {
        // A turn tick lands on the first retained turn observation at or after the event.
        const atOrAfter = points.find(point => point.ts >= compaction.ts)
        const tickX = compact && usesTurns ? turnX(atOrAfter?.index ?? lastIndex) : xFor(compaction.ts, start, end)
        return <path key={`${compaction.ts}-${index}`} d={`M${tickX.toFixed(1)} ${CHART_INSET}V${baseline}`} stroke={status.attention} strokeDasharray="3 3" vectorEffect="non-scaling-stroke"><title>Compaction at {clock.format(compaction.ts)}</title></path>
      })}
    </svg>
    <div data-chart-axis-label {...stylex.props(styles.axis)}>{compact ? <><span>{usesTurns ? `Turn ${firstIndex}` : clock.format(start)}</span><span>{axisLabel}</span><span>{usesTurns ? `Turn ${lastIndex}` : clock.format(end)}</span></> : <><span>{clock.format(start)}</span><span>UTC · ● measured · ○ estimated</span><span>{clock.format(end)}</span></>}</div>
    {!compact && lane.compactions.length > 0 && <ol {...stylex.props(styles.compactions)} aria-label="Compaction context changes">{lane.compactions.map((compaction, index) => <li key={`${compaction.ts}-${index}`}><time dateTime={new Date(compaction.ts).toISOString()}>{clock.format(compaction.ts)}</time>{compaction.before!==null&&compaction.after!==null?` · ${Math.round(compaction.before)}% → ${Math.round(compaction.after)}%`:compaction.before!==null?` · Before ${Math.round(compaction.before)}%`:compaction.after!==null?` · After ${Math.round(compaction.after)}%`:''}</li>)}</ol>}
    {points.some(point=>point.estimated)&&<p {...stylex.props(styles.note)}>Hollow points estimate context from billed input and cache tokens.</p>}
  </div>
}

/** Text-shaped ink retains the populated wrapping without displaying expected observations. */
export const ContextPanelPending = ({ compact = false, footprint }: { readonly compact?: boolean; readonly footprint: ContextPanelFootprint }): ReactNode => <div aria-hidden="true" inert {...stylex.props(styles.root)}>
  <div {...stylex.props(styles.header)}><span><span {...stylex.props(styles.pendingText)}>{footprint.summary}</span></span><span {...stylex.props(styles.note)}><span {...stylex.props(styles.pendingText)}>{footprint.source}</span></span></div>
  <DetailsSkeletonPlot compact={compact} />
  <div {...stylex.props(styles.axis)}>{footprint.axis.map((label, index) => <span key={index}><span {...stylex.props(styles.pendingText)}>{label}</span></span>)}</div>
  {!compact && footprint.compactions.length > 0 && <ol {...stylex.props(styles.compactions)}>{footprint.compactions.map((label, index) => <li key={index}><span {...stylex.props(styles.pendingText)}>{label}</span></li>)}</ol>}
  {footprint.estimated && <p {...stylex.props(styles.note)}><span {...stylex.props(styles.pendingText)}>Hollow points estimate context from billed input and cache tokens.</span></p>}
</div>

const styles = stylex.create({
  root: { display: 'flex', flexDirection: 'column', gap: space.xs, minWidth: 0, color: text.fg, fontSize: type.metaSize, lineHeight: type.metaLeading },
  header: { display: 'flex', flexWrap: 'wrap', justifyContent: 'space-between', gap: space.xs, fontVariantNumeric: 'tabular-nums' },
  chart: { display: 'block', width: '100%', height: 'auto', aspectRatio: '320 / 112', overflow: 'visible' },
  compact: { aspectRatio: '320 / 64' },
  note: { margin: 0, color: text.fgMuted, fontSize: type.denseSize },
  pendingText: { color: surface.transparent, backgroundImage: `linear-gradient(${border.borderStrong}, ${border.borderStrong})`, backgroundSize: `100% ${space.md}`, backgroundPosition: 'center', backgroundRepeat: 'no-repeat', boxDecorationBreak: 'clone' },
  axis: { display: 'flex', justifyContent: 'space-between', gap: space.xs, color: text.fgMuted, fontSize: type.microSize, fontVariantNumeric: 'tabular-nums' },
  compactions: { listStyle: 'none', margin: 0, padding: 0, color: text.fgMuted, fontSize: type.denseSize },
})
